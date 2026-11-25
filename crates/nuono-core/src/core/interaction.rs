//! 通过任务局部上下文（task-local）传递受信任的指令按钮归属信息，不放入客户端可控的 JSON 载荷中。
use std::future::Future;

tokio::task_local! {
    static COMMAND_OWNER: Option<(String, super::plugin_host::interactions::InteractionEpoch)>;
}

pub(crate) async fn with_command_owner<F: Future>(
    owner: Option<(String, super::plugin_host::interactions::InteractionEpoch)>,
    future: F,
) -> F::Output {
    COMMAND_OWNER.scope(owner, future).await
}

pub(crate) fn is_command() -> bool {
    COMMAND_OWNER
        .try_with(|owner| owner.is_some())
        .unwrap_or(false)
}

pub(crate) fn allows_plugin(plugin: Option<&str>) -> bool {
    COMMAND_OWNER
        .try_with(|owner| {
            owner
                .as_ref()
                .is_none_or(|(owner, epoch)| Some(owner.as_str()) == plugin && epoch.is_current())
        })
        .unwrap_or(true)
}

pub(crate) fn is_current() -> bool {
    COMMAND_OWNER
        .try_with(|owner| owner.as_ref().is_none_or(|(_, epoch)| epoch.is_current()))
        .unwrap_or(true)
}

/// 使用与聊天消息相同的适配器身份归一化规则构造宿主合成的领域事件，
/// 确保按钮回调与普通聊天消息共用同一个准入限流键（admission key）。
pub(crate) fn message_domain(event: &serde_json::Value) -> Option<crate::domain::DomainEvent> {
    use super::platform::{MessageType, SenderInfo, UnifiedEvent, UnifiedMessage, UnifiedSegment};
    use serde_json::json;
    let route: super::RuntimeBotRoute = serde_json::from_value(event.get("route")?.clone()).ok()?;
    let actor = super::runtime_outbound::runtime_identity(event, "user_id")?;
    let group = super::runtime_outbound::runtime_identity(event, "group_id");
    let channel = super::runtime_outbound::runtime_identity(event, "channel_id");
    let chat = super::runtime_outbound::runtime_identity(event, "chat_id");
    let message_type = match event.get("message_type")?.as_str()? {
        "private" => MessageType::Private,
        "group" => MessageType::Group,
        "channel" => MessageType::Channel,
        _ => return None,
    };
    let mut raw = event.clone();
    // 以下字段已与签名令牌中记录的目标身份核对一致。
    raw["sender"]["user_id"] = json!(actor);
    match route.platform.as_str() {
        "telegram" => {
            raw["from"] = json!({"id":actor.parse::<i64>().ok()?});
            raw["chat"] = json!({"id":chat.as_ref().or(group.as_ref()).or(channel.as_ref()).unwrap_or(&actor).parse::<i64>().ok()?});
        }
        "discord" => {
            raw["author"] = json!({"id":actor});
        }
        "feishu" => {
            raw["app_id"] = json!(route.bot_self_id);
            raw["open_id"] = json!(actor);
        }
        "qq" | "qq_official" => {
            raw["id"] = event["message_id"].clone();
            raw["author_user_openid"] = json!(actor);
            raw["native_user_openid"] = json!(actor);
            if let Some(group) = &group {
                raw["group_openid"] = json!(group);
                raw["author"] = json!({"member_openid":actor});
            }
        }
        _ => {}
    }
    let text = event
        .get("raw_message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let unified = UnifiedEvent::Message(UnifiedMessage {
        message_id: super::runtime_outbound::runtime_identity(event, "message_id")?,
        self_id: route.bot_self_id.parse().unwrap_or(0),
        user_id: actor.parse().unwrap_or(0),
        group_id: group.as_ref().and_then(|group| group.parse().ok()),
        content: text.clone(),
        message_type,
        sub_type: String::new(),
        sender: SenderInfo {
            user_id: actor,
            nickname: None,
            card: None,
            role: None,
        },
        time: 0,
        raw,
        segments: vec![UnifiedSegment::Text { text }],
    });
    let bot = super::runtime_transport::route_identity(&route).ok()?;
    crate::domain::DomainEventNormalizer::new(bot)
        .normalize(&unified, crate::clock::db_now())
        .ok()
}
