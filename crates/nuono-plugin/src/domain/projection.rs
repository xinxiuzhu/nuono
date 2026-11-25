use super::{ConversationRef, DomainEvent, DomainEventKind};
use crate::core::model::MessageSegment;
use serde_json::{Value, json};

/// 将权威领域事件投影为运行时事件总线与插件消费的 OneBot 结构 JSON。
///
/// 字符串原生 ID 始终原样写入，并附带平台、实例与会话上下文。
pub fn project_legacy_runtime_event(event: &DomainEvent, protocol: &str) -> Value {
    let platform = event.bot.platform().as_str();
    let self_id = event.bot.account_id.value().as_str();
    let mut value = match &event.kind {
        DomainEventKind::Message(message) => {
            let message_type = conversation_type(event.conversation.as_ref());
            let mut value = json!({
                "post_type": "message",
                "message_type": message_type,
                "sub_type": "",
                "self_id": self_id,
                "user_id": message.sender.value().as_str(),
                "message_id": message.message_id.as_str(),
                "message": message.segments.iter().map(segment_json).collect::<Vec<_>>(),
                "raw_message": alt_message(&message.segments),
                "sender": {
                    "user_id": message.sender.value().as_str(),
                },
                "raw": event.raw,
            });
            apply_conversation(&mut value, event.conversation.as_ref());
            value
        }
        DomainEventKind::Notice {
            notice_type,
            sub_type,
        } => json!({
            "post_type": "notice",
            "notice_type": notice_type,
            "sub_type": sub_type.as_deref().unwrap_or(""),
            "self_id": self_id,
            "raw": event.raw,
        }),
        DomainEventKind::Request {
            request_type,
            sub_type,
        } => json!({
            "post_type": "request",
            "request_type": request_type,
            "sub_type": sub_type.as_deref().unwrap_or(""),
            "self_id": self_id,
            "raw": event.raw,
        }),
        DomainEventKind::Meta {
            event_type,
            sub_type,
        } => json!({
            "post_type": "meta_event",
            "meta_event_type": event_type,
            "sub_type": sub_type.as_deref().unwrap_or(""),
            "self_id": self_id,
            "raw": event.raw,
        }),
        _ => json!({
            "post_type": "unknown",
            "self_id": self_id,
            "raw": event.raw,
        }),
    };

    let identity = event
        .actor
        .as_ref()
        .and_then(|actor| serde_json::to_value(actor).ok());
    let conversation = event
        .conversation
        .as_ref()
        .and_then(|conversation| serde_json::to_value(conversation).ok());
    value["event_id"] = json!(event.event_id.as_str());
    value["time"] = json!(event.occurred_at.timestamp());
    value["protocol"] = json!(protocol);
    value["platform"] = json!(platform);
    value["bot_instance"] = json!(event.bot.instance_id.as_str());
    value["identity"] = identity.unwrap_or(Value::Null);
    value["conversation"] = conversation.unwrap_or(Value::Null);
    value["domain_event"] = serde_json::to_value(event).unwrap_or(Value::Null);
    value["reply_context"] = reply_context(event);
    value
}

fn conversation_type(conversation: Option<&ConversationRef>) -> &'static str {
    match conversation {
        Some(ConversationRef::Group { .. }) => "group",
        Some(ConversationRef::Channel { .. }) => "channel",
        _ => "private",
    }
}

fn apply_conversation(value: &mut Value, conversation: Option<&ConversationRef>) {
    match conversation {
        Some(ConversationRef::Direct {
            conversation: Some(conversation),
            ..
        }) => {
            value["conversation_id"] = json!(conversation.value().as_str());
        }
        Some(ConversationRef::Group { group }) => {
            value["group_id"] = json!(group.value().as_str());
        }
        Some(ConversationRef::Channel { channel, parent }) => {
            value["channel_id"] = json!(channel.value().as_str());
            if let Some(parent) = parent {
                value["guild_id"] = json!(parent.value().as_str());
            }
        }
        Some(ConversationRef::Direct {
            conversation: None, ..
        })
        | None
        | Some(_) => {}
    }
}

fn reply_context(event: &DomainEvent) -> Value {
    let mut context = serde_json::Map::new();
    context.insert(
        "self_id".to_string(),
        json!(event.bot.account_id.value().as_str()),
    );
    if let Some(actor) = &event.actor {
        context.insert("user_id".to_string(), json!(actor.value().as_str()));
    }
    match event.conversation.as_ref() {
        Some(ConversationRef::Group { group }) => {
            context.insert("group_id".to_string(), json!(group.value().as_str()));
        }
        Some(ConversationRef::Channel { channel, parent }) => {
            context.insert("channel_id".to_string(), json!(channel.value().as_str()));
            if let Some(parent) = parent {
                context.insert("guild_id".to_string(), json!(parent.value().as_str()));
            }
        }
        Some(ConversationRef::Direct {
            conversation: Some(conversation),
            ..
        }) => {
            context.insert(
                "conversation_id".to_string(),
                json!(conversation.value().as_str()),
            );
        }
        Some(ConversationRef::Direct {
            conversation: None, ..
        })
        | None
        | Some(_) => {}
    }
    if let Some(message) = event.message() {
        context.insert("message_id".to_string(), json!(message.message_id.as_str()));
    }
    context.insert("event_id".to_string(), json!(event.event_id.as_str()));
    Value::Object(context)
}

pub(crate) fn segment_json(segment: &MessageSegment) -> Value {
    match segment {
        MessageSegment::Text { text } => json!({ "type": "text", "data": { "text": text } }),
        MessageSegment::Image { source, id } => json!({
            "type": "image",
            "data": { "file": source.as_str(), "file_id": id }
        }),
        MessageSegment::At { user_id } => {
            json!({ "type": "at", "data": { "qq": user_id } })
        }
        MessageSegment::Reply { message_id } => {
            json!({ "type": "reply", "data": { "id": message_id } })
        }
        MessageSegment::File { name, source } => json!({
            "type": "file",
            "data": { "name": name, "file": source.as_str() }
        }),
        MessageSegment::Record { source } => {
            json!({ "type": "record", "data": { "file": source.as_str() } })
        }
        MessageSegment::Video { source } => {
            json!({ "type": "video", "data": { "file": source.as_str() } })
        }
        MessageSegment::Node { messages } => json!({
            "type": "node",
            "data": { "content": messages.iter().map(segment_json).collect::<Vec<_>>() }
        }),
        MessageSegment::Buttons { buttons } => json!({"type":"buttons","data":{"buttons":buttons}}),
        MessageSegment::Markdown { content } => {
            json!({ "type": "markdown", "data": { "content": content } })
        }
        MessageSegment::TemplateMarkdown { template_id, data } => json!({
            "type": "template_markdown",
            "data": { "template_id": template_id, "params": data.get("params").unwrap_or(data), "fallback":data.get("fallback").and_then(Value::as_str).unwrap_or_default() }
        }),
        MessageSegment::Button { id, label, data } => json!({
            "type": "buttons",
            "data": { "buttons": [[crate::domain::ButtonOne::callback_json(label,id,data.clone())]] }
        }),
        MessageSegment::Unknown { kind, data } => {
            json!({ "type": kind, "data": data })
        }
        _ => json!({ "type": "unknown", "data": {} }),
    }
}

fn alt_message(segments: &[MessageSegment]) -> String {
    segments
        .iter()
        .map(|segment| match segment {
            MessageSegment::Text { text } => text.clone(),
            MessageSegment::At { user_id } => format!("@{user_id}"),
            MessageSegment::Image { .. } => "[图片]".to_string(),
            MessageSegment::Reply { .. } => "[回复]".to_string(),
            MessageSegment::File { name, .. } => format!("[文件:{name}]"),
            MessageSegment::Record { .. } => "[语音]".to_string(),
            MessageSegment::Video { .. } => "[视频]".to_string(),
            MessageSegment::Markdown { content } => content.clone(),
            MessageSegment::Buttons { buttons } => buttons
                .iter()
                .flatten()
                .map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join(" / "),
            MessageSegment::TemplateMarkdown { .. } => "[模板 Markdown]".to_string(),
            MessageSegment::Button { label, .. } => label.clone(),
            MessageSegment::Node { .. } => "[转发消息]".to_string(),
            MessageSegment::Unknown { kind, .. } => format!("[{kind}]"),
            _ => "[未知消息]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::FileSource;
    use crate::domain::{
        BotIdentity, ConversationRef, DomainEventKind, DomainMessage, IdentityKind, IdentityRef,
        IdentityScheme, Platform,
    };
    use chrono::{TimeZone, Utc};

    fn identity(kind: IdentityKind, value: &str) -> IdentityRef {
        IdentityRef::new(
            Platform::OneBot,
            kind,
            IdentityScheme::Native,
            value,
            None::<String>,
        )
        .unwrap()
    }

    #[test]
    fn projects_string_ids_without_numeric_downgrade() {
        let actor = identity(IdentityKind::User, "user-string");
        let group = ConversationRef::group(identity(IdentityKind::Group, "group-string")).unwrap();
        let bot =
            BotIdentity::new("instance", identity(IdentityKind::Bot, "bot-string"), None).unwrap();
        let message = DomainMessage::new(
            "message-string",
            vec![MessageSegment::Text {
                text: "hello".to_string(),
            }],
            actor.clone(),
            Value::Null,
        )
        .unwrap();
        let event = DomainEvent::new(
            "event-string",
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            Utc.timestamp_opt(1_700_000_001, 0).unwrap(),
            bot,
            Some(actor),
            Some(group),
            DomainEventKind::Message(message),
            json!({"source": "test"}),
        )
        .unwrap();

        let value = project_legacy_runtime_event(&event, "telegram");
        assert_eq!(value["self_id"], "bot-string");
        assert_eq!(value["user_id"], "user-string");
        assert_eq!(value["group_id"], "group-string");
        assert_eq!(value["message_id"], "message-string");
        assert_eq!(value["protocol"], "telegram");
    }

    #[test]
    fn unknown_segments_are_preserved() {
        let value = segment_json(&MessageSegment::Unknown {
            kind: "custom.segment".to_string(),
            data: json!({"key": "value"}),
        });
        assert_eq!(value["type"], "custom.segment");
        assert_eq!(value["data"]["key"], "value");
    }

    #[test]
    fn file_source_wire_values_remain_available() {
        let value = segment_json(&MessageSegment::Image {
            source: FileSource::Id("file-id".to_string()),
            id: Some("file-id".to_string()),
        });
        assert_eq!(value["data"]["file"], "file-id");
    }
}
