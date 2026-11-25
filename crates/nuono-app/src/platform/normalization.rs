//! 将适配器入站事件 `UnifiedEvent` 组装为领域层 `DomainEvent` 的公共归一化辅助函数。
//!
//! 各平台特有的身份规则与原始 JSON 字段提取路径由对应平台适配器 crate 提供。
use crate::platform::{UnifiedEvent, UnifiedMessage, UnifiedSegment};
use chrono::TimeZone;
pub use chrono::{DateTime, Utc};
pub use nuono_domain::{
    BotIdentity, ConversationRef, DeliveryTarget, DomainEvent, DomainEventKind, DomainMessage,
    DomainModelError, FileSource, IdentityKind, IdentityRef, IdentityScheme, MessageSegment,
    Platform,
};
use serde_json::{Value, json};

/// 协议适配器提供的原始 JSON 字段提取路径（按回退优先级排列）。
#[derive(Debug, Clone, Copy)]
pub struct EventMetadataPaths {
    pub message_id: &'static [&'static [&'static str]],
    pub event_id: &'static [&'static [&'static str]],
    pub occurred_at: &'static [&'static [&'static str]],
}

pub fn normalize_with<F>(
    bot: &BotIdentity,
    event: &UnifiedEvent,
    received_at: DateTime<Utc>,
    paths: EventMetadataPaths,
    message_context: F,
) -> Result<DomainEvent, DomainModelError>
where
    F: FnOnce(&UnifiedMessage) -> Result<(IdentityRef, ConversationRef), DomainModelError>,
{
    let raw = event.raw().cloned().unwrap_or(Value::Null);
    let message_id = match event {
        UnifiedEvent::Message(message) => Some(message_id(message, paths.message_id)?),
        _ => None,
    };
    let event_id = first_id(&raw, paths.event_id)
        .or_else(|| message_id.clone())
        .ok_or(missing("event_id"))?;
    let occurred_at = occurred_at(event, &raw, received_at, paths.occurred_at);

    let (actor, conversation, kind) = match event {
        UnifiedEvent::Message(message) => {
            let (actor, conversation) = message_context(message)?;
            let message = DomainMessage::new(
                message_id.expect("message events have a validated message id"),
                message.segments.iter().map(normalize_segment).collect(),
                actor.clone(),
                message.raw.clone(),
            )?;
            (
                Some(actor),
                Some(conversation),
                DomainEventKind::Message(message),
            )
        }
        UnifiedEvent::Notice(notice) => (
            None,
            None,
            DomainEventKind::Notice {
                notice_type: notice.notice_type.clone(),
                sub_type: optional_text(&notice.sub_type),
            },
        ),
        UnifiedEvent::Request(request) => (
            None,
            None,
            DomainEventKind::Request {
                request_type: request.request_type.clone(),
                sub_type: optional_text(&request.sub_type),
            },
        ),
        UnifiedEvent::Meta(meta) => (
            None,
            None,
            DomainEventKind::Meta {
                event_type: meta.meta_event_type.clone(),
                sub_type: optional_text(&meta.sub_type),
            },
        ),
    };

    DomainEvent::new(
        event_id,
        occurred_at,
        received_at,
        bot.clone(),
        actor,
        conversation,
        kind,
        raw,
    )
}

pub fn identity(
    platform: Platform,
    kind: IdentityKind,
    scheme: IdentityScheme,
    value: String,
    namespace: Option<String>,
) -> Result<IdentityRef, DomainModelError> {
    IdentityRef::new(platform, kind, scheme, value, namespace)
}

pub fn missing(field: &'static str) -> DomainModelError {
    DomainModelError::InvalidValue {
        field,
        expected: "a non-empty native identifier",
    }
}

fn message_id(message: &UnifiedMessage, paths: &[&[&str]]) -> Result<String, DomainModelError> {
    first_id(&message.raw, paths)
        .or_else(|| non_sentinel(&message.message_id))
        .ok_or(missing("message_id"))
}

fn occurred_at(
    event: &UnifiedEvent,
    raw: &Value,
    received_at: DateTime<Utc>,
    paths: &[&[&str]],
) -> DateTime<Utc> {
    paths
        .iter()
        .filter_map(|path| value_at(raw, path))
        .find_map(parse_timestamp)
        .or_else(|| match event {
            UnifiedEvent::Message(message) => timestamp_from_integer(message.time),
            _ => None,
        })
        .filter(|occurred_at| *occurred_at <= received_at)
        .unwrap_or(received_at)
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(number) => number.as_i64().and_then(timestamp_from_integer),
        Value::String(value) => value
            .parse::<i64>()
            .ok()
            .and_then(timestamp_from_integer)
            .or_else(|| {
                DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|time| time.with_timezone(&Utc))
            }),
        _ => None,
    }
}

fn timestamp_from_integer(value: i64) -> Option<DateTime<Utc>> {
    if value <= 0 {
        return None;
    }
    let (seconds, nanos) = if value >= 100_000_000_000 {
        (
            value.div_euclid(1_000),
            value.rem_euclid(1_000) as u32 * 1_000_000,
        )
    } else {
        (value, 0)
    };
    Utc.timestamp_opt(seconds, nanos).single()
}

fn normalize_segment(segment: &UnifiedSegment) -> MessageSegment {
    match segment {
        UnifiedSegment::Text { text } => MessageSegment::Text { text: text.clone() },
        UnifiedSegment::Image { url } => MessageSegment::Image {
            source: FileSource::from_wire(url.clone()),
            id: None,
        },
        UnifiedSegment::At { user_id } => MessageSegment::At {
            user_id: user_id.to_string(),
        },
        UnifiedSegment::AtUser { user_id } => MessageSegment::At {
            user_id: user_id.clone(),
        },
        UnifiedSegment::AtAll => MessageSegment::Unknown {
            kind: "at_all".to_string(),
            data: Value::Null,
        },
        UnifiedSegment::Face { id } => MessageSegment::Unknown {
            kind: "face".to_string(),
            data: json!({ "id": id }),
        },
        UnifiedSegment::Reply { message_id } => MessageSegment::Reply {
            message_id: message_id.clone(),
        },
        UnifiedSegment::File { url, name } => MessageSegment::File {
            name: name.clone(),
            source: FileSource::from_wire(url.clone()),
        },
        UnifiedSegment::Voice { url } => MessageSegment::Record {
            source: FileSource::from_wire(url.clone()),
        },
        UnifiedSegment::Video { url } => MessageSegment::Video {
            source: FileSource::from_wire(url.clone()),
        },
        UnifiedSegment::Custom { type_name, data } => MessageSegment::Unknown {
            kind: type_name.clone(),
            data: data.clone(),
        },
    }
}

fn optional_text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

pub fn nonzero_i64(value: i64) -> Option<i64> {
    (value != 0).then_some(value)
}

pub fn non_sentinel(value: &str) -> Option<String> {
    (!value.is_empty() && value != "0").then(|| value.to_owned())
}

pub fn first_id(raw: &Value, paths: &[&[&str]]) -> Option<String> {
    paths
        .iter()
        .filter_map(|path| value_at(raw, path))
        .find_map(id_to_string)
}

pub fn first_string(raw: &Value, paths: &[&[&str]]) -> Option<String> {
    paths
        .iter()
        .filter_map(|path| value_at(raw, path))
        .find_map(|value| value.as_str().and_then(non_sentinel))
}

pub fn first_i64(raw: &Value, paths: &[&[&str]]) -> Option<i64> {
    paths
        .iter()
        .filter_map(|path| value_at(raw, path))
        .find_map(|value| match value {
            Value::Number(number) => number.as_i64().and_then(nonzero_i64),
            Value::String(value) => value.parse::<i64>().ok().and_then(nonzero_i64),
            _ => None,
        })
}

fn id_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => non_sentinel(value),
        Value::Number(value) => {
            let value = value.to_string();
            (value != "0").then_some(value)
        }
        _ => None,
    }
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(*key))
}
