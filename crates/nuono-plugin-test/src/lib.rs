//! Nuono 插件单元测试工具。
//!
//! 插件作者把它作为 dev-dependency 引入，在无宿主、无真实机器人的情况下测试
//! handler 和 `on_event` 逻辑：
//!
//! ```toml
//! [dev-dependencies]
//! nuono-plugin-test = { path = "../../crates/nuono-plugin-test" }
//! tokio = { version = "1", features = ["macros", "rt"] }
//! ```
//!
//! ```rust,standalone_crate
//! use nuono_plugin::prelude::*;
//! use nuono_plugin_test::{TestBot, group_event};
//!
//! #[nuono_plugin(name = "PingPlugin")]
//! struct PingPlugin;
//!
//! #[nuono_command(pattern = "ping")]
//! async fn ping(bot: &Bot, _event: &Event<'_>, _data: MatchData) -> Result<()> {
//!     bot.send("pong").await?;
//!     Ok(())
//! }
//!
//! async fn ping_replies_pong() -> Result<()> {
//!     let test = TestBot::new().in_group(12345);
//!     test.call_command(&PingPlugin, "ping", &group_event(12345, 1, "ping"))
//!         .await?;
//!
//!     test.assert_sent_texts(&["pong"]);
//!     Ok(())
//! }
//! # fn main() -> Result<()> {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(ping_replies_pong())
//! # }
//! ```
//!
//! `TestBot` 是接了捕获 sink 的真实 [`Bot`]：`bot.send(...)` 等全部出站 action 被
//! 记录，查询型 action（如 `get_group_list`）从预置应答脚本里依次弹出。

mod host;
pub use host::TestHost;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use nuono_plugin::api::bot::{Bot, MessageContext};
use nuono_plugin::api::waiter::WaitManager;
use nuono_plugin::core::event_bus::EventBus;
use nuono_plugin::core::event_kernel::RuntimeActionSink;
use nuono_plugin::core::onebot_client::ApiClient;
use nuono_plugin::core::{RuntimeActionRouter, RuntimeBotRoute};
use nuono_plugin::error::Result as NuonoResult;
use nuono_plugin::plugin::Event;
use serde_json::{Value, json};

/// 记录所有出站 action；查询型 action 既记录自己又弹出预置应答（默认 Null）。
struct CaptureSink {
    actions: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<Value>>,
}

#[async_trait::async_trait]
impl RuntimeActionSink for CaptureSink {
    async fn call_native_file(
        &self,
        request: nuono_plugin::NativeFileSendRequest,
    ) -> NuonoResult<nuono_plugin::FileSendResponse> {
        request.validate()?;
        let value = self
            .call_action(json!({"action":"send_native_file","params":request}))
            .await?;
        serde_json::from_value(value).map_err(|_| {
            nuono_plugin::NuonoError::Validation("scripted native file receipt is invalid".into())
        })
    }

    async fn send_action(&self, action: Value) -> NuonoResult<()> {
        self.actions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(action);
        Ok(())
    }

    async fn call_action(&self, action: Value) -> NuonoResult<Value> {
        self.actions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(action);
        Ok(self
            .replies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or(Value::Null))
    }
}

/// 捕获型测试 Bot：真实 [`Bot`] 实例 + 出站 action 记录。
///
/// 通过 `in_group` / `in_private` 设置回复上下文后，`bot.send(...)` 会记录
/// 对应的 `send_group_msg` / `send_private_msg` action。要测
/// [`Bot::send_routed`](nuono_plugin::api::Bot::send_routed)，先用 [`Self::connect_route`]
/// 把一条 [`RuntimeBotRoute`] 连到同一个捕获 sink。
pub struct TestBot {
    bot: Bot,
    sink: Arc<CaptureSink>,
    router: Arc<RuntimeActionRouter>,
}

impl Default for TestBot {
    fn default() -> Self {
        Self::new()
    }
}

impl TestBot {
    pub fn new() -> Self {
        let router = Arc::new(RuntimeActionRouter::new());
        let sink = Arc::new(CaptureSink {
            actions: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::new()),
        });
        let bot = Bot::new(
            10000,
            Arc::new(EventBus::new()),
            Arc::new(WaitManager::new()),
            Arc::new(ApiClient::new()),
            None,
            Some(sink.clone()),
        )
        .with_action_router(router.clone());
        Self { bot, sink, router }
    }

    /// 把一条实时路由连到捕获 sink，使 `bot.send_routed(target, ...)` 路由到这里。
    ///
    /// 返回注册代，便于测试卸载。路由的 platform/instance/bot 身份需与传入的
    /// [`RuntimeMessageTarget`](nuono_plugin::api::RuntimeMessageTarget) 的 `route` 一致。
    pub fn connect_route(&self, route: RuntimeBotRoute) -> u64 {
        self.router.register(route, self.sink.clone())
    }

    /// 设置群聊回复上下文：`bot.send` 目标为该群。
    pub fn in_group(mut self, group_id: i64) -> Self {
        self.bot.set_current_message(MessageContext {
            self_id: 10000,
            user_id: 1,
            group_id: Some(group_id),
            channel_id: None,
            message_id: "1".to_string(),
            raw_message: String::new(),
            message_type: "group".to_string(),
            reply_context: Value::Null,
        });
        self
    }

    /// 设置私聊回复上下文：`bot.send` 目标为该用户。
    pub fn in_private(mut self, user_id: i64) -> Self {
        self.bot.set_current_message(MessageContext {
            self_id: 10000,
            user_id,
            group_id: None,
            channel_id: None,
            message_id: "1".to_string(),
            raw_message: String::new(),
            message_type: "private".to_string(),
            reply_context: Value::Null,
        });
        self
    }

    /// 传给 handler 的 `&Bot` 句柄。
    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    /// 预置一条查询型 action 的应答（按调用顺序依次弹出）。
    pub fn push_reply(&mut self, reply: Value) {
        self.sink
            .replies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(reply);
    }

    /// 已记录的全部出站 action 快照（含 `action` 名与 `params`）。
    pub fn actions(&self) -> Vec<Value> {
        self.sink
            .actions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 只看发送消息类 action（`send*`）的消息体，断言回复内容时最常用。
    pub fn sent_messages(&self) -> Vec<Value> {
        self.actions()
            .into_iter()
            .filter(|action| {
                action
                    .get("action")
                    .and_then(Value::as_str)
                    .map(|name| name.starts_with("send"))
                    .unwrap_or(false)
            })
            .map(|action| action["params"]["message"].clone())
            .collect()
    }

    /// 提取每条已发送消息的纯文本（按顺序拼接文本段，忽略非文本段）。
    /// 纯文本消息原样保留，仅含图片/附件等媒体的消息会返回空字符串。
    pub fn sent_texts(&self) -> Vec<String> {
        self.sent_messages().iter().map(message_text).collect()
    }

    /// 断言已发送消息的文本列表与预期完全一致（含顺序与条数）。
    pub fn assert_sent_texts(&self, expected: &[&str]) {
        assert_eq!(self.sent_texts(), expected, "unexpected plugin replies");
    }

    /// 断言未发生任何出站消息发送或 API 查询动作。
    pub fn assert_no_actions(&self) {
        assert!(
            self.actions().is_empty(),
            "unexpected plugin actions: {:?}",
            self.actions()
        );
    }

    /// 调用已注册的指令处理函数，保留字符串形式的平台原生身份。
    /// 直接测试指令业务逻辑，跳过宿主的前缀匹配与权限/冷却策略检查。
    pub async fn call_command<P: nuono_plugin::Plugin + ?Sized>(
        &self,
        plugin: &P,
        name: &str,
        value: &(impl nuono_plugin::serde::Serialize + ?Sized),
    ) -> anyhow::Result<()> {
        let value = serde_json::to_value(value)?;
        let event = Event::new(&value);
        let data = nuono_plugin::MatchData::new(event.text().to_string());
        self.call_command_with(plugin, name, &event, data).await
    }

    /// 用显式事件和匹配参数调用已注册的处理函数，保留原生领域身份。
    ///
    /// 不执行前缀匹配、权限或冷却检查，也不解析参数。测试自行传入
    /// `MatchData::new(...).with_args(...)` 和 `Event::with_domain(...)`。
    pub async fn call_command_with<P: nuono_plugin::Plugin + ?Sized>(
        &self,
        plugin: &P,
        name: &str,
        event: &Event<'_>,
        data: nuono_plugin::MatchData,
    ) -> anyhow::Result<()> {
        let command = plugin
            .commands()
            .into_iter()
            .find(|command| {
                command.name == name || command.aliases.iter().any(|alias| alias == name)
            })
            .ok_or_else(|| anyhow::anyhow!("plugin {} has no command {name}", plugin.name()))?;
        if let Some(handler) = command.handler {
            handler.handle(self.bot(), event, data).await
        } else if let Some(handler) = command.message_handler {
            let value = event.raw();
            let message = match serde_json::from_value::<nuono_plugin::Message>(value.clone()) {
                Ok(message) => message,
                Err(_) => nuono_plugin::onebot::parse_message(&serde_json::to_string(value)?)?,
            };
            handler.handle(self.bot(), &message, data).await
        } else {
            Err(anyhow::anyhow!("command {name} has no handler"))
        }
    }

    /// 清空已记录的 action 与应答脚本。
    pub fn clear(&self) {
        self.sink
            .actions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        self.sink
            .replies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}

fn message_text(message: &Value) -> String {
    if let Some(text) = message.as_str() {
        return text.to_string();
    }
    message
        .as_array()
        .into_iter()
        .flatten()
        .filter(|segment| segment["type"] == "text")
        .filter_map(|segment| segment["data"]["text"].as_str())
        .collect()
}

// ── 事件与消息构造 helper ──

/// 构造群消息事件 JSON（可直接传给 `plugin.on_event`，配合 [`event`] 包装）。
pub fn group_event(group_id: i64, user_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message",
        "message_type": "group",
        "self_id": 10000,
        "user_id": user_id,
        "group_id": group_id,
        "message_id": 1,
        "raw_message": text,
        "sender": { "user_id": user_id, "nickname": "tester", "role": "member" },
    })
}

/// 构造私聊消息事件 JSON。
pub fn private_event(user_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message",
        "message_type": "private",
        "self_id": 10000,
        "user_id": user_id,
        "message_id": 1,
        "raw_message": text,
        "sender": { "user_id": user_id, "nickname": "tester" },
    })
}

/// 构造通知事件 JSON（如 `notice_type = "notify"`、`sub_type = "poke"` 的戳一戳）。
pub fn notice_event(notice_type: &str, sub_type: &str, group_id: i64, user_id: i64) -> Value {
    json!({
        "post_type": "notice",
        "notice_type": notice_type,
        "sub_type": sub_type,
        "self_id": 10000,
        "user_id": user_id,
        "group_id": group_id,
    })
}

/// 构造带平台原生字符串 ID 且附带 route/raw 的群消息事件 JSON。
///
/// 供指令（`Command`，handler 拿 `&Event`）测试使用：`bot_self_id` /
/// `user_id` / `group_id` / `message_id` 全部以字符串保真写入，并带上可被
/// [`Event::route()`] 解析的 `route` 与可被 [`Event::raw_event()`] 读取的 `raw`。
/// 这些字段不会经过 OneBot 的数值投影 / 哈希。
///
/// `platform` 同时用作 `route.instance_id`，便于断言 `route()` 回路完全绕开数值 bot API。
pub fn platform_group_event(
    platform: &str,
    bot_self_id: &str,
    user_id: &str,
    group_id: &str,
    message_id: &str,
    text: &str,
) -> Value {
    json!({
        "platform": platform,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "self_id": bot_self_id,
        "user_id": user_id,
        "group_id": group_id,
        "message_id": message_id,
        "raw_message": text,
        "message": [{ "type": "text", "data": { "text": text } }],
        "sender": { "user_id": user_id, "nickname": "tester", "role": "member" },
        "route": {
            "platform": platform,
            "instance_id": platform,
            "bot_self_id": bot_self_id,
        },
        "raw": { "event_id": message_id, "platform": platform },
    })
}

/// 构造带平台原生字符串 ID 且附带 route/raw 的私聊消息事件 JSON。字段语义同
/// [`platform_group_event`]（仅无 `group_id`）。
pub fn platform_private_event(
    platform: &str,
    bot_self_id: &str,
    user_id: &str,
    message_id: &str,
    text: &str,
) -> Value {
    json!({
        "platform": platform,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "self_id": bot_self_id,
        "user_id": user_id,
        "message_id": message_id,
        "raw_message": text,
        "message": [{ "type": "text", "data": { "text": text } }],
        "sender": { "user_id": user_id, "nickname": "tester" },
        "route": {
            "platform": platform,
            "instance_id": platform,
            "bot_self_id": bot_self_id,
        },
        "raw": { "event_id": message_id, "platform": platform },
    })
}

/// 把事件 JSON 包装成 `on_event` 需要的 [`Event`]。
pub fn event(value: &Value) -> Event<'_> {
    Event::new(value)
}

/// 构造触发器 handler 需要的群聊 `Message`（从 OneBot JSON 反序列化）。
pub fn group_message(group_id: i64, user_id: i64, text: &str) -> nuono_plugin::Message {
    serde_json::from_value(json!({
        "time": 0,
        "self_id": 10000,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "message_id": 1,
        "user_id": user_id,
        "group_id": group_id,
        "message": [{ "type": "text", "data": { "text": text } }],
        "raw_message": text,
    }))
    .expect("测试群消息 JSON 必须可反序列化")
}

/// 构造触发器 handler 需要的私聊 `Message`。
pub fn private_message(user_id: i64, text: &str) -> nuono_plugin::Message {
    serde_json::from_value(json!({
        "time": 0,
        "self_id": 10000,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "message_id": 1,
        "user_id": user_id,
        "message": [{ "type": "text", "data": { "text": text } }],
        "raw_message": text,
    }))
    .expect("测试私聊 JSON 必须可反序列化")
}

/// 构造保留完整 Bot、平台和用户 namespace 的原生私聊领域事件。
///
/// 时间和消息编号固定，仅用于单元测试；不调用真实适配器或做账号绑定。
/// 通过 `Event::with_domain(&domain.raw, Some(&domain))` 传给处理函数。
///
/// # Errors
/// Bot 与用户身份不匹配、用户类型不正确时返回错误。
pub fn native_private_event(
    bot: nuono_plugin::domain::BotIdentity,
    actor: nuono_plugin::domain::IdentityRef,
    text: &str,
) -> anyhow::Result<nuono_plugin::domain::DomainEvent> {
    use nuono_plugin::domain::{
        ConversationRef, DomainEvent, DomainEventKind, DomainMessage, MessageSegment,
    };
    let timestamp = "2026-01-01T00:00:00Z".parse()?;
    let raw = json!({
        "post_type": "message", "message_type": "private",
        "platform": bot.platform().as_str(), "self_id": bot.account_id.value(),
        "route": {"platform": bot.platform().as_str(), "instance_id": bot.instance_id, "bot_self_id": bot.account_id.value()},
        "user_id": actor.value(), "message_id": "1", "raw_message": text,
    });
    let message = DomainMessage::new(
        "1",
        vec![MessageSegment::Text { text: text.into() }],
        actor.clone(),
        raw.clone(),
    )?;
    Ok(DomainEvent::new(
        "test-message-1",
        timestamp,
        timestamp,
        bot,
        Some(actor.clone()),
        Some(ConversationRef::direct(actor, None)?),
        DomainEventKind::Message(message),
        raw,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_plugin::api::routed::RuntimeMessageTarget;

    #[tokio::test]
    async fn send_routed_is_captured_and_returns_receipt() {
        let mut test = TestBot::new();
        let route = RuntimeBotRoute::new("qq_official", "instance", "bot");
        test.connect_route(route.clone());
        test.push_reply(json!({
            "message_ids": ["msg-99"],
            "raw": {"ok": true},
        }));

        let target = RuntimeMessageTarget::private(route, "member-openid");
        let receipt = test.bot().send_routed(&target, "hello").await.unwrap();
        assert_eq!(receipt.message_ids, vec!["msg-99".to_string()]);
        assert_eq!(receipt.raw, json!({"ok": true}));

        let actions = test.actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["action"], "send_msg");
        assert_eq!(actions[0]["params"]["openid"], "member-openid");
        assert_eq!(
            actions[0]["params"]["message"],
            json!([{"type": "text", "data": {"text": "hello"}}])
        );
    }
}
