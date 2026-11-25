//! 供宿主流水线测试使用的极简 DTO 适配器测试桩，不涉及网络请求、平台凭据或生产环境编解码。
use nuono_app::platform::*;
use nuono_error::{NuonoError, Result};
use serde_json::{Value, json};
#[derive(Debug)]
pub struct FixtureAdapter;
#[async_trait::async_trait]
impl PlatformAdapter for FixtureAdapter {
    fn platform_name(&self) -> &str {
        "fixture"
    }
    fn platform_version(&self) -> &str {
        "1"
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            send_message: true,
            ..Default::default()
        }
    }
    fn extract_self_id(&self, raw: &str) -> Option<i64> {
        serde_json::from_str::<Value>(raw)
            .ok()?
            .get("self_id")?
            .as_i64()
    }
    fn parse_incoming(&self, raw: &str) -> Result<UnifiedEvent> {
        let raw: Value = serde_json::from_str(raw)?;
        let text = |key: &str| raw[key].as_str().unwrap_or_default().to_owned();
        let num = |key: &str| raw[key].as_i64().unwrap_or_default();
        let self_id = num("self_id");
        match text("post_type").as_str() {
            "notice" => Ok(UnifiedEvent::Notice(UnifiedNotice {
                self_id,
                notice_type: text("notice_type"),
                sub_type: text("sub_type"),
                raw,
            })),
            "request" => Ok(UnifiedEvent::Request(UnifiedRequest {
                self_id,
                request_type: text("request_type"),
                sub_type: text("sub_type"),
                raw,
            })),
            "meta_event" => Ok(UnifiedEvent::Meta(UnifiedMeta {
                self_id,
                meta_event_type: text("meta_event_type"),
                sub_type: text("sub_type"),
                raw,
            })),
            "message" => {
                let segments = raw["message"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| match s["type"].as_str()? {
                        "text" => Some(UnifiedSegment::Text {
                            text: s["data"]["text"].as_str()?.into(),
                        }),
                        "at" if s["data"]["qq"] == "all" => Some(UnifiedSegment::AtAll),
                        "at" => Some(UnifiedSegment::AtUser {
                            user_id: s["data"]["qq"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| s["data"]["qq"].to_string()),
                        }),
                        "image" => Some(UnifiedSegment::Image {
                            url: s["data"]["url"]
                                .as_str()
                                .or_else(|| s["data"]["file"].as_str())?
                                .into(),
                        }),
                        _ => None,
                    })
                    .collect();
                Ok(UnifiedEvent::Message(UnifiedMessage {
                    self_id,
                    user_id: num("user_id"),
                    group_id: raw["group_id"].as_i64(),
                    message_id: raw["message_id"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| raw["message_id"].to_string()),
                    content: text("raw_message"),
                    message_type: match text("message_type").as_str() {
                        "group" => MessageType::Group,
                        "guild" | "channel" => MessageType::Channel,
                        _ => MessageType::Private,
                    },
                    sub_type: text("sub_type"),
                    time: num("time"),
                    sender: SenderInfo {
                        user_id: num("user_id").to_string(),
                        nickname: raw["sender"]["nickname"].as_str().map(str::to_owned),
                        card: raw["sender"]["card"].as_str().map(str::to_owned),
                        role: raw["sender"]["role"].as_str().map(str::to_owned),
                    },
                    segments,
                    raw,
                }))
            }
            _ => Err(NuonoError::Platform("unsupported fixture".into())),
        }
    }
    fn build_action(&self, action: &Action) -> Value {
        match action {
            Action::SendMessage { target, message } => match target {
                ActionTarget::Private { user_id } => {
                    json!({"action":"send_private_msg","params":{"user_id":user_id,"message":self.segments_to_protocol(message)}})
                }
                ActionTarget::Group { group_id } => {
                    json!({"action":"send_group_msg","params":{"group_id":group_id,"message":self.segments_to_protocol(message)}})
                }
                _ => Value::Null,
            },
            _ => Value::Null,
        }
    }
    fn build_send_message(&self, ty: &MessageType, id: i64, text: &str) -> Value {
        self.build_action(&Action::SendMessage {
            target: if *ty == MessageType::Group {
                ActionTarget::Group { group_id: id }
            } else {
                ActionTarget::Private { user_id: id }
            },
            message: vec![UnifiedSegment::Text { text: text.into() }],
        })
    }
    fn segments_to_protocol(&self, segments: &[UnifiedSegment]) -> Value {
        Value::Array(
            segments
                .iter()
                .map(|segment| match segment {
                    UnifiedSegment::Text { text } => json!({"type":"text","data":{"text":text}}),
                    _ => Value::Null,
                })
                .collect(),
        )
    }
}
