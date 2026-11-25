use super::*;
use nuono_app::{managed_messages::MessageType, transport::RuntimeBotRoute};
use nuono_core::domain::{MessageSegment, OutboundAction};

fn route(instance: &str) -> RuntimeBotRoute {
    RuntimeBotRoute {
        platform: "onebot".into(),
        instance_id: instance.into(),
        bot_self_id: "42".into(),
    }
}
fn legacy() -> SendMessagePayload {
    serde_json::from_value(json!({"bot_id":"42","target_type":"private","target_id":"123","message":"**hello**","message_type":"markdown"})).unwrap()
}
#[test]
fn legacy_messages_preserve_markdown_and_reject_ambiguous_instances() {
    let request = normalize_message(&legacy(), &[route("first")]).unwrap();
    assert_eq!(request.message_type, MessageType::Markdown);
    let OutboundAction::SendMessage { segments, .. } = request.action().unwrap() else {
        panic!("send")
    };
    assert!(matches!(&segments[0], MessageSegment::Markdown {content} if content=="**hello**"));
    assert!(normalize_message(&legacy(), &[route("first"), route("second")]).is_err());
    assert!(normalize_message(&legacy(), &[]).is_err());
}
#[test]
fn exact_route_and_native_ids_are_preserved_without_numeric_conversion() {
    let body: SendMessagePayload = serde_json::from_value(json!({
        "route":{"platform":"qq_official","instance_id":"second","bot_self_id":"0007"},
        "target":{"type":"actor","actor":{"platform":"qq_official","kind":"user","scheme":"open_id","value":"00AbC","namespace":"0007"}},
        "message":"hello","message_type":"text"
    })).unwrap();
    let request = normalize_message(&body, &[]).unwrap();
    assert_eq!(request.route.instance_id, "second");
    assert_eq!(
        request.target.identities().next().unwrap().value().as_str(),
        "00AbC"
    );
}
#[test]
fn partial_route_and_overlong_unicode_messages_are_rejected() {
    let mut body = legacy();
    body.route = Some(route("first"));
    assert!(normalize_message(&body, &[route("first")]).is_err());
    body.route = None;
    body.message = "你".repeat(2000);
    assert!(normalize_message(&body, &[route("first")]).is_ok());
    body.message.push('你');
    assert!(normalize_message(&body, &[route("first")]).is_err());
    assert!(
        serde_json::from_value::<SendMessagePayload>(json!({"message":"hi","message_type":"html"}))
            .is_err()
    );
}
