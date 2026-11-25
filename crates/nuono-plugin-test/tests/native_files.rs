use nuono_plugin::{RuntimeBotRoute, domain::*, media::FileDelivery};
use nuono_plugin_test::TestBot;
use serde_json::json;

fn actor(scheme: IdentityScheme) -> IdentityRef {
    IdentityRef::new(
        Platform::Feishu,
        IdentityKind::User,
        scheme,
        "opaque_user",
        Some("app-a"),
    )
    .unwrap()
}
#[tokio::test]
async fn native_file_scripts_capture_complete_identity_and_preserve_confirmed_receipts() {
    let mut test = TestBot::new();
    let route = RuntimeBotRoute::new("feishu", "instance", "app-a");
    test.connect_route(route.clone());
    let target = DeliveryTarget::conversation(
        ConversationRef::direct(
            actor(IdentityScheme::UnionId),
            Some(
                IdentityRef::new(
                    Platform::Feishu,
                    IdentityKind::Conversation,
                    IdentityScheme::ChatId,
                    "opaque_chat",
                    Some("app-a"),
                )
                .unwrap(),
            ),
        )
        .unwrap(),
    )
    .unwrap();
    test.push_reply(json!({"status":"sent","message_ids":["000042"],"raw":{"native":"000042"}}));
    let result = test
        .bot()
        .send_native_file_routed(
            &route,
            &target,
            b"{}".to_vec(),
            "records.json",
            Some("application/json"),
        )
        .await
        .unwrap();
    assert!(
        matches!(result, FileDelivery::Sent { receipt } if receipt.message_ids == ["000042"] && receipt.raw == json!({"native":"000042"}))
    );
    assert_eq!(
        test.actions(),
        [json!({"action":"send_native_file","params":{
            "target":target,"base64":"e30=","file_name":"records.json","content_type":"application/json"
        }})]
    );
}
#[tokio::test]
async fn native_file_scripted_unsupported_and_malformed_receipts_are_distinct() {
    let mut test = TestBot::new();
    let route = RuntimeBotRoute::new("feishu", "instance", "app-a");
    test.connect_route(route.clone());
    let target = DeliveryTarget::actor(actor(IdentityScheme::UserId)).unwrap();
    test.push_reply(json!({"status":"unsupported"}));
    test.push_reply(json!({"status":"sent","message_ids":[],"raw":{}}));
    test.push_reply(json!({"status":"unsupported","message_ids":["fake"]}));
    assert_eq!(
        test.bot()
            .send_native_file_routed(&route, &target, vec![1], "records.json", None)
            .await
            .unwrap(),
        FileDelivery::Unsupported
    );
    for _ in 0..2 {
        assert!(
            test.bot()
                .send_native_file_routed(&route, &target, vec![1], "records.json", None)
                .await
                .is_err()
        );
    }
    assert_eq!(test.actions().len(), 3);
}
