//! TestBot 自身的回路测试：捕获发送、上下文路由、事件构造 helper。

use nuono_plugin::prelude::*;
use nuono_plugin_test::{TestBot, event, group_message, notice_event, private_event};

struct PingHandler;

#[async_trait]
impl TriggerHandler for PingHandler {
    async fn handle(&self, bot: &Bot, _msg: &Message, _data: MatchData) -> Result<()> {
        bot.send("pong!").await?;
        Ok(())
    }
}

#[tokio::test]
async fn group_send_is_captured_as_group_action() {
    let test = TestBot::new().in_group(42);
    let msg = group_message(42, 7, "ping");
    PingHandler
        .handle(test.bot(), &msg, MatchData::new("ping".into()))
        .await
        .unwrap();

    let sent = test.sent_messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0][0]["type"], "text");
    assert_eq!(sent[0][0]["data"]["text"], "pong!");

    let actions = test.actions();
    let action = actions[0].get("action").and_then(|v| v.as_str()).unwrap();
    assert!(action.starts_with("send_group"), "群上下文应走群发 action");
}

#[tokio::test]
async fn private_send_routes_to_private_action() {
    let test = TestBot::new().in_private(7);
    let msg = nuono_plugin_test::private_message(7, "ping");
    PingHandler
        .handle(test.bot(), &msg, MatchData::new("ping".into()))
        .await
        .unwrap();

    let actions = test.actions();
    let action = actions[0]
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    assert!(
        action.starts_with("send_private"),
        "私聊上下文应走私聊 action: {action}"
    );
}

#[tokio::test]
async fn event_helpers_expose_expected_fields() {
    let private = private_event(7, "hi");
    let evt = event(&private);
    assert!(evt.is_private());
    assert_eq!(evt.user_id(), 7);
    assert_eq!(evt.text(), "hi");

    let poke_value = notice_event("notify", "poke", 42, 7);
    let poke = event(&poke_value);
    assert_eq!(poke.post_type(), "notice");
    assert_eq!(poke.notice_type(), Some("notify"));
    assert_eq!(poke.sub_type(), "poke");
}
