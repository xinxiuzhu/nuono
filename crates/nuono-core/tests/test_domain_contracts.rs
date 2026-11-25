use nuono_core::core::platform::ActionTarget as LegacyActionTarget;
use nuono_core::domain::{
    DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, LegacyProjectionError, Platform,
};

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

fn actor_target(actor: IdentityRef) -> DeliveryTarget {
    DeliveryTarget::actor(actor).unwrap()
}

#[test]
fn legacy_projection_only_accepts_onebot_and_reports_overflow() {
    let telegram_target = actor_target(numeric_identity(
        Platform::Telegram,
        IdentityKind::User,
        "-42",
    ));
    assert!(matches!(
        LegacyActionTarget::try_from(telegram_target),
        Err(LegacyProjectionError::WrongPlatform {
            expected: Platform::OneBot,
            actual: Platform::Telegram,
        })
    ));

    let overflowing_target = actor_target(identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Native,
        "9223372036854775808",
        None,
    ));
    assert!(matches!(
        LegacyActionTarget::try_from(overflowing_target),
        Err(LegacyProjectionError::NativeIdOverflow { value })
            if value == "9223372036854775808"
    ));

    let onebot_target = actor_target(identity(
        Platform::OneBot,
        IdentityKind::User,
        IdentityScheme::Native,
        "123",
        None,
    ));
    assert!(matches!(
        LegacyActionTarget::try_from(onebot_target),
        Ok(LegacyActionTarget::Private { user_id: 123 })
    ));
}

#[test]
fn old_import_paths_accept_domain_messages_without_conversion() {
    let wire = serde_json::json!({
        "type": "node",
        "data": {"messages": [
            {"type": "text", "data": {"text": "hello"}},
            {"type": "image", "data": {
                "source": {"type": "url", "value": "https://example.com/image.png"},
                "id": null
            }},
            {"type": "unknown", "data": {"kind": "custom", "data": {"key": 42}}}
        ]}
    });
    let message: nuono_domain::MessageSegment = serde_json::from_value(wire.clone()).unwrap();
    let old_path: nuono_core::core::model::MessageSegment = message.clone();
    let facade: nuono_core::domain::message::MessageSegment = old_path;
    assert_eq!(facade, message);
    assert_eq!(serde_json::to_value(facade).unwrap(), wire);
}

#[test]
fn onebot_conversion_produces_shared_domain_segments() {
    let onebot = nuono_core::onebot::MessageSegment::Reply {
        data: nuono_core::onebot::ReplyData {
            id: "18446744073709551615".to_owned(),
        },
    };
    let segment: nuono_domain::MessageSegment = onebot.into();
    assert_eq!(
        segment,
        nuono_domain::MessageSegment::Reply {
            message_id: "18446744073709551615".to_owned()
        }
    );
}
