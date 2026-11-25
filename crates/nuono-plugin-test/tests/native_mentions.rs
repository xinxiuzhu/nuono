use nuono_plugin::{MessageBuilder, RuntimeBotRoute, RuntimeMessageTarget};
use nuono_plugin_test::TestBot;
use serde_json::json;

#[tokio::test]
async fn routed_notifications_preserve_native_user_mentions_and_numeric_compatibility() {
    for (platform, user_id) in [
        ("onebot", "00123"),
        ("discord", "18446744073709551615"),
        ("feishu", "ou_member_openid"),
        ("qq", "opaque_user_openid"),
        ("telegram", "4503599627370495"),
    ] {
        let test = TestBot::new();
        let route = RuntimeBotRoute::new(platform, format!("{platform}-instance"), "bot-native");
        test.connect_route(route.clone());
        let group_id = if platform == "telegram" {
            "-1001234567890"
        } else {
            "group-native"
        };
        let target = RuntimeMessageTarget::group(route, group_id);
        test.bot()
            .send_routed_notify(
                &target,
                MessageBuilder::new()
                    .at_user(user_id)
                    .text(" hello")
                    .at(123),
            )
            .await
            .unwrap();
        let actions = test.actions();
        assert_eq!(actions.len(), 1);
        let key = match platform {
            "discord" => "channel_id",
            "feishu" => "chat_id",
            _ => "group_id",
        };
        let expected = if platform == "telegram" {
            json!(-1001234567890_i64)
        } else {
            json!(group_id)
        };
        assert_eq!(actions[0]["params"][key], expected);
        assert_eq!(
            actions[0]["params"]["message"],
            json!([
                {"type":"at","data":{"qq":user_id}},
                {"type":"text","data":{"text":" hello"}},
                {"type":"at","data":{"qq":123}}
            ])
        );
    }
}
