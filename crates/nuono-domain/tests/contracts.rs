use chrono::{TimeZone, Utc};
use nuono_domain::{
    BotIdentity, ConversationRef, DeliveryTarget, DomainEvent, DomainEventKind, DomainMessage,
    IdentityKind, IdentityRef, IdentityScheme, MessageSegment, NativeId, OutboundAction, Platform,
    QueryOperation, SessionKey, SessionScope,
};
use serde_json::json;

fn identity(
    platform: Platform,
    kind: IdentityKind,
    scheme: IdentityScheme,
    value: &str,
    namespace: Option<&str>,
) -> IdentityRef {
    IdentityRef::new(platform, kind, scheme, value, namespace.map(str::to_owned)).unwrap()
}

fn numeric_identity(platform: Platform, kind: IdentityKind, value: &str) -> IdentityRef {
    let scheme = match platform {
        Platform::Discord => IdentityScheme::Snowflake,
        _ => IdentityScheme::Numeric,
    };
    identity(platform, kind, scheme, value, None)
}

fn actor_target(actor: IdentityRef) -> nuono_domain::DeliveryTarget {
    nuono_domain::DeliveryTarget::actor(actor).unwrap()
}

#[test]
fn delivery_target_identities_include_the_complete_conversation_context() {
    let id = |kind, value| identity(Platform::OneBot, kind, IdentityScheme::Native, value, None);
    let user = id(IdentityKind::User, "001-user");
    let direct = id(IdentityKind::Conversation, "direct-context");
    let group = id(IdentityKind::Group, "002-group");
    let channel = id(IdentityKind::Channel, "003-channel");
    let parent = id(IdentityKind::Organization, "parent-context");
    let mut cases = vec![
        (DeliveryTarget::actor(user.clone()).unwrap(), vec![&user]),
        (
            DeliveryTarget::conversation(ConversationRef::group(group.clone()).unwrap()).unwrap(),
            vec![&group],
        ),
    ];
    for context in [None, Some(direct.clone())] {
        let mut expected = vec![&user];
        if context.is_some() {
            expected.push(&direct);
        }
        cases.push((
            DeliveryTarget::conversation(ConversationRef::direct(user.clone(), context).unwrap())
                .unwrap(),
            expected,
        ));
    }
    for context in [None, Some(parent.clone())] {
        let mut expected = vec![&channel];
        if context.is_some() {
            expected.push(&parent);
        }
        cases.push((
            DeliveryTarget::conversation(
                ConversationRef::channel(channel.clone(), context.clone()).unwrap(),
            )
            .unwrap(),
            expected.clone(),
        ));
        cases.push((
            DeliveryTarget::channel(channel.clone(), context).unwrap(),
            expected,
        ));
    }
    for (target, expected) in cases {
        assert_eq!(
            target.identities().collect::<Vec<_>>(),
            expected,
            "{target:?}"
        );
    }
}

#[test]
fn native_id_boundary_contract_is_property_like() {
    for length in 0..=513 {
        let value = "x".repeat(length);
        let result = NativeId::new(value.clone());
        if length == 0 || length > 512 {
            assert!(result.is_err(), "length {length} should be rejected");
        } else {
            assert_eq!(result.unwrap().as_str(), value);
        }
    }

    for control in (0u32..=0x1f).chain(std::iter::once(0x7f)) {
        let character = char::from_u32(control).unwrap();
        let value = format!("prefix{character}suffix");
        assert!(NativeId::new(value).is_err(), "control U+{control:04X}");
    }

    for value in ["", " ", "\t", "\n", " \t "] {
        assert!(NativeId::new(value).is_err(), "empty-like value {value:?}");
    }

    for value in [
        "é",
        "界",
        "🙂",
        "e\u{301}",
        &"é".repeat(256),
        &format!("{}a", "é".repeat(255)),
        &format!("{}a", "é".repeat(256)),
    ] {
        let result = NativeId::new(value.to_owned());
        if value.len() <= 512 {
            assert_eq!(result.unwrap().as_str(), value);
        } else {
            assert!(result.is_err(), "{} bytes should be rejected", value.len());
        }
    }
}

#[test]
fn identity_scope_includes_platform_kind_scheme_and_namespace() {
    let base = identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Native,
        "same-value",
        None,
    );
    let other_platform = identity(
        Platform::Other("other-platform".to_owned()),
        IdentityKind::User,
        IdentityScheme::Native,
        "same-value",
        None,
    );
    let other_kind = identity(
        Platform::OneBot,
        IdentityKind::Bot,
        IdentityScheme::Native,
        "same-value",
        None,
    );
    let other_scheme = identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Other("other-scheme".to_owned()),
        "same-value",
        None,
    );
    let namespaced = identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Native,
        "same-value",
        Some("namespace-a"),
    );

    assert_ne!(base, other_platform);
    assert_ne!(base, other_kind);
    assert_ne!(base, other_scheme);
    assert_ne!(base, namespaced);

    let feishu_app_a = identity(
        Platform::Feishu,
        IdentityKind::User,
        IdentityScheme::OpenId,
        "ou_same-open-id",
        Some("app-a"),
    );
    let feishu_app_b = identity(
        Platform::Feishu,
        IdentityKind::User,
        IdentityScheme::OpenId,
        "ou_same-open-id",
        Some("app-b"),
    );
    assert_eq!(feishu_app_a.value().as_str(), feishu_app_b.value().as_str());
    assert_ne!(feishu_app_a, feishu_app_b);
}

#[test]
fn platform_native_id_boundaries_are_preserved() {
    let discord = identity(
        Platform::Discord,
        IdentityKind::User,
        IdentityScheme::Snowflake,
        &u64::MAX.to_string(),
        None,
    );
    assert_eq!(discord.value().as_str(), u64::MAX.to_string());

    let telegram = numeric_identity(Platform::Telegram, IdentityKind::User, "-1001234567890");
    assert_eq!(telegram.value().as_str(), "-1001234567890");

    let onebot = identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Native,
        "user-string-id",
        None,
    );
    assert_eq!(onebot.value().as_str(), "user-string-id");
}

#[test]
fn domain_event_and_session_key_serde_roundtrip() {
    let actor = numeric_identity(Platform::Telegram, IdentityKind::User, "-42");
    let conversation = ConversationRef::direct(actor.clone(), None).unwrap();
    let bot_account = numeric_identity(Platform::Telegram, IdentityKind::Bot, "100");
    let bot = BotIdentity::new("telegram-instance", bot_account, None).unwrap();
    let occurred_at = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
    let received_at = Utc.timestamp_opt(1_700_000_001, 0).single().unwrap();
    let message = DomainMessage::new(
        "message-1",
        vec![MessageSegment::text("hello")],
        actor.clone(),
        json!({"update_id": 1}),
    )
    .unwrap();
    let event = DomainEvent::new(
        "event-1",
        occurred_at,
        received_at,
        bot.clone(),
        Some(actor.clone()),
        Some(conversation.clone()),
        DomainEventKind::Message(message),
        json!({"source": "contract-test"}),
    )
    .unwrap();

    let event_json = serde_json::to_string(&event).unwrap();
    let event_restored: DomainEvent = serde_json::from_str(&event_json).unwrap();
    assert_eq!(event_restored, event);

    let session_key = SessionKey::new(
        bot,
        SessionScope::ActorInConversation {
            actor,
            conversation,
        },
    )
    .unwrap();
    let session_json = serde_json::to_string(&session_key).unwrap();
    let session_restored: SessionKey = serde_json::from_str(&session_json).unwrap();
    assert_eq!(session_restored, session_key);
}

#[test]
fn outbound_action_json_validation_rejects_invalid_payloads() {
    let onebot_actor = identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Native,
        "user-1",
        None,
    );
    let target = actor_target(onebot_actor);

    let empty_segments = OutboundAction::SendMessage {
        target: target.clone(),
        segments: Vec::new(),
        reply_to: None,
    };
    let empty_segments_json = serde_json::to_value(empty_segments).unwrap();
    assert!(serde_json::from_value::<OutboundAction>(empty_segments_json).is_err());

    let empty_reaction = OutboundAction::React {
        target,
        message_id: NativeId::new("message-1").unwrap(),
        reaction: String::new(),
    };
    let empty_reaction_json = serde_json::to_value(empty_reaction).unwrap();
    assert!(serde_json::from_value::<OutboundAction>(empty_reaction_json).is_err());

    let telegram_group = ConversationRef::group(numeric_identity(
        Platform::Telegram,
        IdentityKind::Group,
        "-1001",
    ))
    .unwrap();
    let discord_actor = identity(
        Platform::Discord,
        IdentityKind::User,
        IdentityScheme::Snowflake,
        "42",
        None,
    );
    let cross_platform_query = OutboundAction::Query {
        query: QueryOperation::GroupMember {
            conversation: Box::new(telegram_group),
            actor: discord_actor,
        },
    };
    let cross_platform_query_json = serde_json::to_value(cross_platform_query).unwrap();
    assert!(serde_json::from_value::<OutboundAction>(cross_platform_query_json).is_err());
}

#[test]
fn query_json_shape_is_preserved_with_an_indirect_conversation() {
    let conversation = ConversationRef::group(numeric_identity(
        Platform::OneBot,
        IdentityKind::Group,
        "123",
    ))
    .unwrap();
    let actor = numeric_identity(Platform::OneBot, IdentityKind::User, "456");
    let expected_member = json!({
        "type": "group_member",
        "data": { "conversation": conversation, "actor": actor }
    });
    let queries = [
        (QueryOperation::GroupList, json!({ "type": "group_list" })),
        (QueryOperation::FriendList, json!({ "type": "friend_list" })),
        (
            QueryOperation::GroupMember {
                conversation: Box::new(conversation),
                actor,
            },
            expected_member,
        ),
        (QueryOperation::LoginInfo, json!({ "type": "login_info" })),
        (
            QueryOperation::Raw {
                name: "custom_query".to_string(),
                params: json!({ "page": 2 }),
            },
            json!({ "type": "raw", "data": {
                "name": "custom_query", "params": { "page": 2 }
            }}),
        ),
    ];
    for (query, expected) in queries {
        assert_eq!(serde_json::to_value(&query).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<QueryOperation>(expected).unwrap(),
            query
        );
    }
}

#[test]
fn query_inline_size_is_smaller_than_its_largest_conversation_payload() {
    let query_bytes = std::mem::size_of::<QueryOperation>();
    let conversation_bytes = std::mem::size_of::<ConversationRef>();
    eprintln!("QueryOperation: {query_bytes} bytes; ConversationRef: {conversation_bytes} bytes");
    assert!(query_bytes < conversation_bytes);
}
