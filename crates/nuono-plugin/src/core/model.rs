use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

use crate::core::permission::PermissionLevel;

pub use nuono_domain::{FileSource, MessageSegment, MessageTarget, UserType};

/// 从适配器传入核心的规范入站消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageReceive {
    pub bot_id: String,
    pub bot_self_id: String,
    pub msg_id: String,
    pub user_type: UserType,
    pub group_id: Option<String>,
    pub user_id: String,
    #[serde(default)]
    pub sender: HashMap<String, Value>,
    #[serde(default)]
    pub permission: PermissionLevel,
    #[serde(default)]
    pub content: Vec<MessageSegment>,
}

impl Default for MessageReceive {
    fn default() -> Self {
        Self {
            bot_id: "Bot".to_string(),
            bot_self_id: String::new(),
            msg_id: String::new(),
            user_type: UserType::Group,
            group_id: None,
            user_id: String::new(),
            sender: HashMap::new(),
            permission: PermissionLevel::Everyone,
            content: Vec::new(),
        }
    }
}

/// 在业务内核中使用的规范事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub bot_id: String,
    pub bot_self_id: String,
    pub msg_id: String,
    pub user_type: UserType,
    pub group_id: Option<String>,
    pub user_id: String,
    #[serde(default)]
    pub sender: HashMap<String, Value>,
    #[serde(default)]
    pub permission: PermissionLevel,
    pub content: Vec<MessageSegment>,
    pub ws_bot_id: Option<String>,
    pub task_id: String,
    pub real_bot_id: String,
    pub raw_text: String,
    pub command: String,
    pub text: String,
    pub image: Option<String>,
    pub image_list: Vec<String>,
    pub image_id: Option<String>,
    pub image_id_list: Vec<String>,
    pub at: Option<String>,
    pub at_list: Vec<String>,
    pub is_tome: bool,
    pub reply: Option<String>,
    pub file_name: Option<String>,
    pub file: Option<String>,
    pub file_type: Option<FilePayloadType>,
    pub regex_group: Vec<String>,
    pub regex_dict: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FilePayloadType {
    Url,
    Base64,
    Path,
    Raw,
}

impl Event {
    pub fn from_receive(msg: MessageReceive) -> Self {
        let real_bot_id = msg.bot_id.clone();
        let bot_id = msg
            .bot_id
            .split_once(':')
            .map(|(id, _)| id.to_string())
            .unwrap_or_else(|| msg.bot_id.clone());

        let mut event = Self {
            bot_id,
            bot_self_id: msg.bot_self_id,
            msg_id: msg.msg_id,
            user_type: msg.user_type,
            group_id: msg.group_id,
            user_id: msg.user_id,
            sender: msg.sender,
            permission: msg.permission,
            content: msg.content,
            ws_bot_id: None,
            task_id: String::new(),
            real_bot_id,
            raw_text: String::new(),
            command: String::new(),
            text: String::new(),
            image: None,
            image_list: Vec::new(),
            image_id: None,
            image_id_list: Vec::new(),
            at: None,
            at_list: Vec::new(),
            is_tome: false,
            reply: None,
            file_name: None,
            file: None,
            file_type: None,
            regex_group: Vec::new(),
            regex_dict: HashMap::new(),
        };

        if event.user_type.is_direct() {
            event.is_tome = true;
        }

        for segment in &event.content {
            match segment {
                MessageSegment::Text { text } => {
                    let part = text.trim();
                    event.raw_text.push_str(part);
                    event.text.push_str(part);
                }
                MessageSegment::At { user_id } => {
                    if *user_id == event.bot_self_id {
                        event.is_tome = true;
                    } else {
                        event.at = Some(user_id.clone());
                        event.at_list.push(user_id.clone());
                    }
                }
                MessageSegment::Image { source, id } => {
                    let value = source.as_str().to_string();
                    event.image = Some(value.clone());
                    event.image_list.push(value);
                    if let Some(id) = id {
                        event.image_id = Some(id.clone());
                        event.image_id_list.push(id.clone());
                    }
                }
                MessageSegment::Reply { message_id } => {
                    event.reply = Some(message_id.clone());
                }
                MessageSegment::File { name, source } => {
                    event.file_name = Some(name.clone());
                    event.file = Some(source.as_str().to_string());
                    event.file_type = Some(match source {
                        FileSource::Url(_) => FilePayloadType::Url,
                        FileSource::Base64(_) => FilePayloadType::Base64,
                        FileSource::Path(_) => FilePayloadType::Path,
                        FileSource::Id(_) | FileSource::Raw(_) | _ => FilePayloadType::Raw,
                    });
                }
                _ => {}
            }
        }

        event
    }

    pub fn session_id(&self) -> String {
        let bid = if self.bot_id.is_empty() {
            "0"
        } else {
            &self.bot_id
        };
        if self.user_type.is_direct() {
            let uid = if self.user_id.is_empty() {
                "0"
            } else {
                &self.user_id
            };
            format!("bot:{bid}:private:{uid}")
        } else {
            let gid = self.group_id.as_deref().unwrap_or("0");
            format!("bot:{bid}:group:{gid}")
        }
    }
}

/// 规范的外发消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageSend {
    pub bot_id: String,
    pub bot_self_id: String,
    pub msg_id: String,
    pub target: MessageTarget,
    pub content: Vec<MessageSegment>,
}

/// 提交给任务执行者的工作的元数据。
#[derive(Debug, Clone)]
pub struct TaskContext {
    pub name: String,
    pub task_id: String,
    pub priority: i8,
    pub created_at: Instant,
}

impl TaskContext {
    pub fn new(name: impl Into<String>, task_id: impl Into<String>, priority: i8) -> Self {
        Self {
            name: name.into(),
            task_id: task_id.into(),
            priority,
            created_at: Instant::now(),
        }
    }
}

impl Eq for TaskContext {}

impl PartialEq for TaskContext {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.created_at == other.created_at
    }
}

impl PartialOrd for TaskContext {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskContext {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap 默认先弹出最大值；这里反转比较顺序，使数值越小（或创建时间越早）的任务优先级越高。
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.created_at.cmp(&self.created_at))
    }
}

impl From<crate::onebot::Message> for MessageReceive {
    fn from(message: crate::onebot::Message) -> Self {
        match message {
            crate::onebot::Message::GroupMessage(msg) => {
                let sender = group_sender_to_map(&msg.sender);
                Self {
                    bot_id: msg.self_id.to_string(),
                    bot_self_id: msg.self_id.to_string(),
                    msg_id: msg.message_id.to_string(),
                    user_type: UserType::Group,
                    group_id: Some(msg.group_id.to_string()),
                    user_id: msg.user_id.to_string(),
                    sender,
                    permission: PermissionLevel::Everyone,
                    content: msg.message.into_iter().map(MessageSegment::from).collect(),
                }
            }
            crate::onebot::Message::PrivateMessage(msg) => {
                let sender = private_sender_to_map(&msg.sender);
                Self {
                    bot_id: msg.self_id.to_string(),
                    bot_self_id: msg.self_id.to_string(),
                    msg_id: msg.message_id.to_string(),
                    user_type: UserType::Direct,
                    group_id: None,
                    user_id: msg.user_id.to_string(),
                    sender,
                    permission: PermissionLevel::Everyone,
                    content: msg.message.into_iter().map(MessageSegment::from).collect(),
                }
            }
        }
    }
}

fn private_sender_to_map(sender: &crate::onebot::PrivateMessageSender) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    if let Some(v) = sender.user_id {
        map.insert("user_id".to_string(), Value::from(v));
    }
    if let Some(v) = &sender.nickname {
        map.insert("nickname".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &sender.sex {
        map.insert("sex".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = sender.age {
        map.insert("age".to_string(), Value::from(v));
    }
    map
}

fn group_sender_to_map(sender: &crate::onebot::GroupMessageSender) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    if let Some(v) = sender.user_id {
        map.insert("user_id".to_string(), Value::from(v));
    }
    if let Some(v) = &sender.nickname {
        map.insert("nickname".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &sender.card {
        map.insert("card".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &sender.sex {
        map.insert("sex".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = sender.age {
        map.insert("age".to_string(), Value::from(v));
    }
    if let Some(v) = &sender.area {
        map.insert("area".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &sender.level {
        map.insert("level".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &sender.role {
        map.insert("role".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &sender.title {
        map.insert("title".to_string(), Value::from(v.clone()));
    }
    map
}

impl From<crate::onebot::MessageSegment> for MessageSegment {
    fn from(segment: crate::onebot::MessageSegment) -> Self {
        match segment {
            crate::onebot::MessageSegment::Text { data } => Self::Text { text: data.text },
            crate::onebot::MessageSegment::At { data } => {
                let user_id = match data.qq {
                    crate::onebot::AtType::All => "all".to_string(),
                    crate::onebot::AtType::Id(id) => id,
                };
                Self::At { user_id }
            }
            crate::onebot::MessageSegment::Image { data } => Self::Image {
                source: FileSource::from_wire(
                    data.url
                        .filter(|url| !url.trim().is_empty())
                        .unwrap_or(data.file),
                ),
                id: None,
            },
            crate::onebot::MessageSegment::Record { data } => Self::Record {
                source: FileSource::from_wire(data.url.unwrap_or(data.file)),
            },
            crate::onebot::MessageSegment::Video { data } => Self::Video {
                source: FileSource::from_wire(data.url.unwrap_or(data.file)),
            },
            crate::onebot::MessageSegment::Reply { data } => Self::Reply {
                message_id: data.id,
            },
            crate::onebot::MessageSegment::Node { data } => Self::Node {
                messages: data.content.into_iter().map(Self::from).collect(),
            },
            crate::onebot::MessageSegment::File { data } => Self::File {
                name: data.name,
                source: FileSource::from_wire(data.file),
            },
            other => Self::Unknown {
                kind: onebot_segment_kind(&other).to_string(),
                data: serde_json::to_value(other).unwrap_or(Value::Null),
            },
        }
    }
}

fn onebot_segment_kind(segment: &crate::onebot::MessageSegment) -> &'static str {
    match segment {
        crate::onebot::MessageSegment::Text { .. } => "text",
        crate::onebot::MessageSegment::At { .. } => "at",
        crate::onebot::MessageSegment::Image { .. } => "image",
        crate::onebot::MessageSegment::Record { .. } => "record",
        crate::onebot::MessageSegment::Video { .. } => "video",
        crate::onebot::MessageSegment::Face { .. } => "face",
        crate::onebot::MessageSegment::Reply { .. } => "reply",
        crate::onebot::MessageSegment::Forward { .. } => "forward",
        crate::onebot::MessageSegment::Node { .. } => "node",
        crate::onebot::MessageSegment::File { .. } => "file",
        crate::onebot::MessageSegment::Poke { .. } => "poke",
        crate::onebot::MessageSegment::Xml { .. } => "xml",
        crate::onebot::MessageSegment::Json { .. } => "json",
        crate::onebot::MessageSegment::Music { .. } => "music",
        crate::onebot::MessageSegment::Share { .. } => "share",
        crate::onebot::MessageSegment::Contact { .. } => "contact",
        crate::onebot::MessageSegment::Location { .. } => "location",
        crate::onebot::MessageSegment::Rps { .. } => "rps",
        crate::onebot::MessageSegment::Dice { .. } => "dice",
        crate::onebot::MessageSegment::Shake { .. } => "shake",
        crate::onebot::MessageSegment::Anonymous { .. } => "anonymous",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_onebot_image_keeps_its_download_url() {
        let event = crate::onebot::parse_message(
            &serde_json::json!({
                "post_type":"message", "message_type":"private", "sub_type":"friend",
                "self_id":10000, "user_id":30000, "message_id":42,
                "message":[{"type":"image", "data":{"file":"opaque-id", "url":"https://example.com/image.png"}}]
            })
            .to_string(),
        )
        .unwrap();
        let crate::onebot::Message::PrivateMessage(message) = event else {
            panic!("expected a private image message");
        };
        let segment = message.message.into_iter().next().unwrap();
        assert!(
            matches!(MessageSegment::from(segment), MessageSegment::Image {source: FileSource::Url(url), ..} if url == "https://example.com/image.png")
        );
    }

    #[test]
    fn event_from_receive_extracts_core_fields() {
        let receive = MessageReceive {
            bot_id: "onebot:main".to_string(),
            bot_self_id: "10000".to_string(),
            msg_id: "42".to_string(),
            user_type: UserType::Group,
            group_id: Some("20000".to_string()),
            user_id: "30000".to_string(),
            content: vec![
                MessageSegment::Text {
                    text: " /ping ".to_string(),
                },
                MessageSegment::At {
                    user_id: "10000".to_string(),
                },
                MessageSegment::Image {
                    source: FileSource::Url("https://example.com/a.png".to_string()),
                    id: Some("img1".to_string()),
                },
            ],
            ..Default::default()
        };

        let event = Event::from_receive(receive);

        assert_eq!(event.bot_id, "onebot");
        assert_eq!(event.real_bot_id, "onebot:main");
        assert_eq!(event.raw_text, "/ping");
        assert!(event.is_tome);
        assert_eq!(event.image_id.as_deref(), Some("img1"));
        assert_eq!(event.session_id(), "bot:onebot:group:20000");
    }

    #[test]
    fn direct_events_are_to_me() {
        let event = Event::from_receive(MessageReceive {
            user_type: UserType::Direct,
            user_id: "30000".to_string(),
            bot_id: "bot".to_string(),
            content: vec![MessageSegment::Text {
                text: "hello".to_string(),
            }],
            ..Default::default()
        });

        assert!(event.is_tome);
        assert_eq!(event.session_id(), "bot:bot:private:30000");
    }
}
