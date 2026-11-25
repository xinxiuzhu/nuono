//! 入站统一事件到运行时消息 JSON（OneBot 结构）的投影转换。
//!
//! 见各函数文档：这是有损投影，平台原生 ID 只在 `raw` 等字段中完整保留。

use crate::core::platform::{MessageType, UnifiedEvent, UnifiedSegment};

/// 将协议无关事件投影为运行时消息处理器使用的 OneBot 结构 JSON。
///
/// 这是有损投影而非无损序列化：Discord guild 消息把 `channel_id` 暂借为
/// `group_id`，Feishu 把 `chat_id` 暂借为 `channel_id`；字符串 snowflake/open_id
/// 仅在可安全解析时进入 i64 字段。平台原值必须从 `raw`、`channel_id`、`chat_id`
/// 或 `guild_id` 读取，不能把投影字段当作可逆的协议 ID。
pub fn unified_event_to_runtime_json(platform: &str, event: &UnifiedEvent) -> serde_json::Value {
    unified_event_to_runtime_json_with_route(platform, event, None)
}

pub fn unified_event_to_runtime_json_with_route(
    platform: &str,
    event: &UnifiedEvent,
    route: Option<&super::RuntimeBotRoute>,
) -> serde_json::Value {
    let mut value = match event {
        UnifiedEvent::Message(msg) => {
            let message_type = match msg.message_type {
                MessageType::Private => "private",
                MessageType::Group => "group",
                MessageType::Channel => "channel",
            };
            let message = unified_segments_to_onebot_json(&msg.segments);
            let raw_channel_id = msg.raw.get("channel_id").and_then(|v| v.as_str());
            let raw_guild_id = msg.raw.get("guild_id").and_then(|v| v.as_str());
            let raw_chat_id = msg
                .raw
                .get("message")
                .and_then(|v| v.get("chat_id"))
                .and_then(|v| v.as_str())
                .or_else(|| msg.raw.get("chat_id").and_then(|v| v.as_str()));
            let channel_id = raw_channel_id.and_then(|id| id.parse::<i64>().ok());
            let guild_id = raw_guild_id.and_then(|id| id.parse::<i64>().ok());
            let compatible_group_id = if platform.eq_ignore_ascii_case("discord") {
                channel_id.or(msg.group_id)
            } else {
                msg.group_id
            };
            let native_self_id = msg.raw.get("app_id").and_then(|value| value.as_str());
            let is_feishu = platform.eq_ignore_ascii_case("feishu");
            let is_qq = matches!(platform.to_ascii_lowercase().as_str(), "qq" | "qq_official");
            let is_discord = platform.eq_ignore_ascii_case("discord");
            let native_user_id = if is_discord {
                Some(msg.sender.user_id.as_str())
            } else if is_qq {
                ["openid", "native_user_openid", "author_user_openid"]
                    .iter()
                    .find_map(|key| msg.raw.get(*key).and_then(|value| value.as_str()))
                    .or_else(|| {
                        (!msg.sender.user_id.trim().is_empty())
                            .then_some(msg.sender.user_id.as_str())
                    })
            } else {
                msg.raw
                    .get("open_id")
                    .and_then(|value| value.as_str())
                    .or_else(|| {
                        (!msg.sender.user_id.trim().is_empty())
                            .then_some(msg.sender.user_id.as_str())
                    })
            };
            let native_group_id = if is_qq {
                ["group_openid", "native_group_openid"]
                    .iter()
                    .find_map(|key| msg.raw.get(*key).and_then(|value| value.as_str()))
            } else {
                msg.raw.get("chat_id").and_then(|value| value.as_str())
            };
            let use_native_identity = is_discord || is_feishu || is_qq;
            let mut value = serde_json::json!({
                "post_type": "message",
                "platform": platform,
                "message_type": message_type,
                "sub_type": msg.sub_type,
                "message_id": msg.message_id,
                "self_id": if is_feishu {
                    serde_json::json!(native_self_id.unwrap_or_default())
                } else {
                    serde_json::json!(msg.self_id)
                },
                "user_id": if use_native_identity {
                    serde_json::json!(native_user_id.unwrap_or_default())
                } else {
                    serde_json::json!(msg.user_id)
                },
                "time": msg.time,
                "message": message,
                "raw_message": msg.content,
                "sender": {
                    "user_id": msg.sender.user_id,
                    "nickname": msg.sender.nickname,
                    "card": msg.sender.card,
                    "role": msg.sender.role,
                },
                "raw": msg.raw,
            });
            if use_native_identity {
                if message_type != "private"
                    && let Some(group_id) = native_group_id.or(raw_channel_id)
                {
                    value["group_id"] = serde_json::json!(group_id);
                }
            } else if let Some(group_id) = compatible_group_id {
                value["group_id"] = serde_json::json!(group_id);
            }
            if let Some(channel_id) = raw_channel_id {
                value["channel_id"] = serde_json::json!(channel_id);
            }
            if let Some(chat_id) = raw_chat_id {
                value["chat_id"] = serde_json::json!(chat_id);
            }
            if let Some(guild_id) = raw_guild_id {
                value["guild_id"] = serde_json::json!(guild_id);
            } else if let Some(guild_id) = guild_id {
                value["guild_id"] = serde_json::json!(guild_id);
            }

            let raw_object = msg.raw.as_object();
            for key in ["openid", "group_openid", "event_id", "qq_target_type"] {
                if let Some(native_value) = raw_object.and_then(|raw| raw.get(key)) {
                    value[key] = native_value.clone();
                }
            }
            if is_qq {
                if let Some(user_id) = native_user_id {
                    value["openid"] = serde_json::json!(user_id);
                }
                if let Some(group_id) = native_group_id {
                    value["group_openid"] = serde_json::json!(group_id);
                }
                value["qq_target_type"] = serde_json::json!(if message_type == "group" {
                    "group"
                } else {
                    "c2c"
                });
            }
            let mut reply_context = serde_json::Map::new();
            for key in [
                "platform",
                "message_type",
                "message_id",
                "user_id",
                "group_id",
                "channel_id",
                "guild_id",
                "chat_id",
                "openid",
                "group_openid",
                "event_id",
                "qq_target_type",
            ] {
                if let Some(context_value) = value.get(key) {
                    reply_context.insert(key.to_string(), context_value.clone());
                }
            }
            value["reply_context"] = serde_json::Value::Object(reply_context);
            value
        }
        UnifiedEvent::Notice(notice) if notice.notice_type == "interaction" => {
            let mut value = notice.raw.clone();
            if !value.is_object() {
                value = serde_json::json!({});
            }
            value["post_type"] = serde_json::json!("interaction");
            value["platform"] = serde_json::json!(platform);
            value["self_id"] = serde_json::json!(notice.self_id);
            value
        }
        UnifiedEvent::Notice(notice) => serde_json::json!({
            "post_type": "notice",
            "platform": platform,
            "self_id": notice.self_id,
            "notice_type": notice.notice_type,
            "sub_type": notice.sub_type,
            "raw": notice.raw,
        }),
        UnifiedEvent::Request(request) => serde_json::json!({
            "post_type": "request",
            "platform": platform,
            "self_id": request.self_id,
            "request_type": request.request_type,
            "sub_type": request.sub_type,
            "raw": request.raw,
        }),
        UnifiedEvent::Meta(meta) => serde_json::json!({
            "post_type": "meta_event",
            "platform": platform,
            "self_id": meta.self_id,
            "meta_event_type": meta.meta_event_type,
            "sub_type": meta.sub_type,
            "raw": meta.raw,
        }),
    };
    inject_runtime_route(&mut value, route);
    value
}

pub fn inject_runtime_route(event: &mut serde_json::Value, route: Option<&super::RuntimeBotRoute>) {
    let Some(route) = route else {
        return;
    };
    event["platform"] = serde_json::json!(route.platform);
    event["route"] = serde_json::json!(route);
    if let Some(reply_context) = event
        .get_mut("reply_context")
        .and_then(serde_json::Value::as_object_mut)
    {
        reply_context.insert("platform".to_string(), serde_json::json!(route.platform));
        reply_context.insert("route".to_string(), serde_json::json!(route));
    }
}

/// 将统一消息段编码为运行时消费的 OneBot 消息段 JSON。
///
/// `Custom` 只透传类型名和 JSON 数据，平台特有实体、富文本范围、附件元数据等不会
/// 被重建；需要这些信息的消费者应读取事件的 `raw`。
fn unified_segments_to_onebot_json(segments: &[UnifiedSegment]) -> serde_json::Value {
    nuono_app::platform::outbound::compatibility_segments(segments)
}

#[cfg(test)]
mod tests {
    use super::inject_runtime_route;
    use crate::core::RuntimeBotRoute;
    use crate::core::platform::{MessageType, SenderInfo, UnifiedEvent, UnifiedMessage};
    use serde_json::json;

    #[test]
    fn discord_native_projection_preserves_large_snowflakes() {
        let user_id = "4358232439484032348234234";
        let channel_id = "9358232439484032348234234";
        let guild_id = "7358232439484032348234234";
        let event = UnifiedEvent::Message(UnifiedMessage {
            message_id: "message-1".to_string(),
            self_id: 42,
            user_id: 0,
            group_id: None,
            content: "/ping".to_string(),
            message_type: MessageType::Group,
            sub_type: String::new(),
            sender: SenderInfo {
                user_id: user_id.to_string(),
                nickname: Some("tester".to_string()),
                card: None,
                role: None,
            },
            time: 0,
            raw: json!({
                "user": {"id": user_id},
                "channel_id": channel_id,
                "guild_id": guild_id,
            }),
            segments: vec![],
        });

        let projected = super::unified_event_to_runtime_json("discord", &event);
        assert_eq!(projected["user_id"], user_id);
        assert_eq!(projected["group_id"], channel_id);
        assert_eq!(projected["channel_id"], channel_id);
        assert_eq!(projected["guild_id"], guild_id);
        assert_eq!(projected["reply_context"]["user_id"], user_id);
    }

    #[test]
    fn route_metadata_preserves_exact_strings() {
        let route = RuntimeBotRoute::new("qq_official", "instance:primary", "app:not-numeric");
        let mut event = json!({
            "platform": "legacy",
            "reply_context": {"platform": "legacy"}
        });
        inject_runtime_route(&mut event, Some(&route));

        assert_eq!(event["platform"], "qq_official");
        assert_eq!(event["route"]["instance_id"], "instance:primary");
        assert_eq!(event["route"]["bot_self_id"], "app:not-numeric");
        assert_eq!(
            event["reply_context"]["route"]["bot_self_id"],
            "app:not-numeric"
        );
    }
}
