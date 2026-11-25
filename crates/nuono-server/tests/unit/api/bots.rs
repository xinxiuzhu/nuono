use super::*;

#[test]
fn probe_response_keeps_flat_legacy_fields_and_runtime_evidence() {
    for status in ["online", "offline", "unknown", "degraded"] {
        let bot: BotStatus = serde_json::from_value(json!({
            "bot_id": "123456", "nickname": "测试机器人", "status": status,
            "ip_address": "127.0.0.1", "last_heartbeat": "", "platform": "onebot",
            "transport_connected": true, "status_reason": "运行时证据", "last_seen": null,
        }))
        .unwrap();
        let value = serde_json::to_value(BotPingResponse::from(bot)).unwrap();
        assert_eq!(value["online"], status == "online");
        assert_eq!(value["status"], status);
        assert_eq!(value["bot_id"], "123456");
        assert_eq!(value["nickname"], "测试机器人");
        assert_eq!(value["status_reason"], "运行时证据");
        assert!(value.get("bot").is_none());
    }
}

#[test]
fn test_message_requires_a_valid_platform_receipt() {
    for data in [
        json!({"message_id": 123}),
        json!({"message_id": -123}),
        json!({"message_id": "123"}),
    ] {
        assert!(confirmed_message_receipt(&data));
    }
    for data in [
        json!(null),
        json!({}),
        json!({"message_id": ""}),
        json!({"message_id": true}),
        json!({"status": "ok", "data": {"message_id": 123}}),
    ] {
        assert!(!confirmed_message_receipt(&data));
    }
}

#[test]
fn legacy_online_without_a_live_transport_is_not_confirmed_online() {
    let bot: BotStatus = serde_json::from_value(json!({
        "bot_id": "123456", "nickname": null, "status": "online",
        "ip_address": "", "last_heartbeat": "",
    }))
    .unwrap();
    assert!(!BotPingResponse::from(bot).online);
}

#[test]
fn service_warning_allows_an_explicit_send_test_but_unknown_or_offline_does_not() {
    for (status, reason, connected, allowed) in [
        ("online", "status_confirmed", true, true),
        ("degraded", "platform_unhealthy", true, true),
        ("degraded", "heartbeat_expired", true, false),
        ("degraded", "probe_failed", true, false),
        ("offline", "platform_offline", true, false),
        ("unknown", "status_unconfirmed", true, false),
        ("degraded", "platform_unhealthy", false, false),
    ] {
        let bot: BotStatus = serde_json::from_value(json!({
            "bot_id":"42", "status":status, "status_reason":reason,
            "transport_connected":connected, "ip_address":"", "last_heartbeat":"",
        }))
        .unwrap();
        assert_eq!(
            can_send_test_message(&bot),
            allowed,
            "{status}/{reason}/{connected}"
        );
    }
}
