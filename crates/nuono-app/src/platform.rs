//! 跨平台适配器入站事件投影、基础动作模型与适配器接口定义。
pub mod canonical;
pub mod normalization;
pub mod outbound;

use async_trait::async_trait;
use nuono_error::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// 统一事件类型（协议无关）

#[derive(Debug, Clone)]
pub enum UnifiedEvent {
    Message(UnifiedMessage),
    Notice(UnifiedNotice),
    Request(UnifiedRequest),
    Meta(UnifiedMeta),
}

impl UnifiedEvent {
    pub fn message_type(&self) -> Option<&str> {
        match self {
            UnifiedEvent::Message(m) => match m.message_type {
                MessageType::Private => Some("private"),
                MessageType::Group => Some("group"),
                MessageType::Channel => Some("channel"),
            },
            UnifiedEvent::Notice(_) => Some("notice"),
            UnifiedEvent::Request(_) => Some("request"),
            UnifiedEvent::Meta(_) => Some("meta_event"),
        }
    }

    pub fn raw(&self) -> Option<&Value> {
        match self {
            UnifiedEvent::Message(m) => Some(&m.raw),
            UnifiedEvent::Notice(n) => Some(&n.raw),
            UnifiedEvent::Request(r) => Some(&r.raw),
            UnifiedEvent::Meta(m) => Some(&m.raw),
        }
    }

    pub fn self_id(&self) -> Option<i64> {
        match self {
            UnifiedEvent::Message(m) => Some(m.self_id),
            UnifiedEvent::Notice(n) => Some(n.self_id),
            UnifiedEvent::Request(r) => Some(r.self_id),
            UnifiedEvent::Meta(m) => Some(m.self_id),
        }
    }

    pub fn user_id(&self) -> Option<i64> {
        match self {
            UnifiedEvent::Message(m) => Some(m.user_id),
            _ => self
                .raw()
                .and_then(|r| r.get("user_id").and_then(|v| v.as_i64())),
        }
    }

    pub fn group_id(&self) -> Option<i64> {
        match self {
            UnifiedEvent::Message(m) => m.group_id,
            _ => self
                .raw()
                .and_then(|r| r.get("group_id").and_then(|v| v.as_i64())),
        }
    }

    pub fn is_message(&self) -> bool {
        matches!(self, UnifiedEvent::Message(_))
    }

    pub fn is_notice(&self) -> bool {
        matches!(self, UnifiedEvent::Notice(_))
    }

    pub fn as_message(&self) -> Option<&UnifiedMessage> {
        match self {
            UnifiedEvent::Message(m) => Some(m),
            _ => None,
        }
    }
}

/// 跨平台处理所需的最小消息投影。
///
/// 数字 ID 字段服务于既有 OneBot 风格权限、会话和触发器接口；平台原生 ID 可能是
/// 字符串、超出可解析范围或具有不同作用域。此时适配器会使用 `0`/`None` 等 sentinel
/// 或稳定哈希，并把权威值保存在 [`Self::raw`] 与 [`SenderInfo::user_id`] 中。
#[derive(Debug, Clone)]
pub struct UnifiedMessage {
    pub message_id: String,
    pub self_id: i64,
    pub user_id: i64,
    pub group_id: Option<i64>,
    pub content: String,
    pub message_type: MessageType,
    pub sub_type: String,
    pub sender: SenderInfo,
    pub time: i64,
    /// 未经统一模型重建的平台负载，是恢复原生 ID、富文本和未建模字段的权威来源。
    pub raw: Value,
    /// 消息段列表（协议无关、有损）
    pub segments: Vec<UnifiedSegment>,
}

impl UnifiedMessage {
    pub fn is_private(&self) -> bool {
        self.message_type == MessageType::Private
    }

    pub fn is_group(&self) -> bool {
        self.message_type == MessageType::Group
    }

    pub fn session_id(&self) -> String {
        match self.group_id {
            Some(gid) => format!("{}:{}:{}", self.self_id, gid, self.user_id),
            None => format!("{}:private:{}", self.self_id, self.user_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageType {
    #[serde(rename = "private")]
    Private,
    #[serde(rename = "group")]
    Group,
    #[serde(rename = "channel")]
    Channel,
}

#[derive(Debug, Clone)]
pub struct SenderInfo {
    pub user_id: String,
    pub nickname: Option<String>,
    pub card: Option<String>,
    pub role: Option<String>,
}

/// 协议无关的消息段子集。
///
/// 该枚举刻意只覆盖 runtime 的公共能力，不保证平台往返等价。无法建模的段应放入
/// [`UnifiedSegment::Custom`]，但文本实体范围、交互组件和附件权限等上下文仍只能从
/// `UnifiedMessage::raw` 取得。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum UnifiedSegment {
    Text {
        text: String,
    },
    Image {
        url: String,
    },
    At {
        user_id: i64,
    },
    /// 原生字符串身份；数字兼容段 `At` 保持原有接口与存储形状。
    AtUser {
        user_id: String,
    },
    AtAll,
    Face {
        id: i64,
    },
    Reply {
        message_id: String,
    },
    File {
        url: String,
        name: String,
    },
    Voice {
        url: String,
    },
    Video {
        url: String,
    },
    Custom {
        type_name: String,
        data: Value,
    },
}

#[derive(Debug, Clone)]
pub struct UnifiedNotice {
    pub self_id: i64,
    pub notice_type: String,
    pub sub_type: String,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct UnifiedRequest {
    pub self_id: i64,
    pub request_type: String,
    pub sub_type: String,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct UnifiedMeta {
    pub self_id: i64,
    pub meta_event_type: String,
    pub sub_type: String,
    pub raw: Value,
}

// 与协议无关的操作类型

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "params")]
pub enum Action {
    SendMessage {
        target: ActionTarget,
        message: Vec<UnifiedSegment>,
    },
    DeleteMessage {
        message_id: String,
    },
    SendLike {
        user_id: i64,
        times: i32,
    },
    SetGroupBan {
        group_id: i64,
        user_id: i64,
        duration: i32,
    },
    SetGroupKick {
        group_id: i64,
        user_id: i64,
        reject_add_request: bool,
    },
    SetGroupCard {
        group_id: i64,
        user_id: i64,
        card: String,
    },
    SetGroupLeave {
        group_id: i64,
        is_dismiss: bool,
    },
    GetGroupList,
    GetFriendList,
    GetGroupMemberInfo {
        group_id: i64,
        user_id: i64,
    },
    GetLoginInfo,
    SetGroupAdmin {
        group_id: i64,
        user_id: i64,
        enable: bool,
    },
    HandleRequest {
        request_type: String,
        flag: String,
        approve: bool,
        remark: Option<String>,
    },
    Custom {
        action: String,
        params: Value,
    },
}

/// `Action` 的逻辑发送目标。
///
/// 数字 ID 主要用于 OneBot 协议动作编码；非纯数字 ID 的平台（如 Discord snowflake、
/// 飞书 `open_id`）通过运行时事件中的原生会话标识与平台发送通道寻址。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionTarget {
    Private { user_id: i64 },
    Group { group_id: i64 },
    Channel { guild_id: i64, channel_id: i64 },
}

// 适配器能力

#[derive(Debug, Clone, Default)]
pub struct AdapterCapabilities {
    pub send_message: bool,
    pub delete_message: bool,
    pub send_like: bool,
    pub group_admin: bool,
    pub group_ban: bool,
    pub group_kick: bool,
    pub get_group_list: bool,
    pub get_friend_list: bool,
    pub handle_request: bool,
    pub send_image: bool,
    pub send_voice: bool,
    pub send_video: bool,
    pub custom_action: bool,
    pub markdown: bool,
    pub template_markdown: bool,
    pub buttons: bool,
    pub interactions: bool,
}

// Platform Adapter Trait（协议无关）

#[async_trait]
pub trait PlatformAdapter: Send + Sync + std::fmt::Debug {
    fn platform_name(&self) -> &str;
    fn platform_version(&self) -> &str {
        "unknown"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    /// 将平台负载投影为统一事件；成功不意味着该投影可无损还原平台负载。
    fn parse_incoming(&self, raw: &str) -> Result<UnifiedEvent>;
    /// 从入站负载提取当前 bot ID。
    ///
    /// 该钩子主要用于 OneBot 连接握手。主动拉取或 webhook 平台可能没有每包 bot ID，
    /// 因而合法地返回 `None`，由适配器配置构造统一事件的 `self_id`。
    fn extract_self_id(&self, raw: &str) -> Option<i64>;

    /// 构建平台 action 描述；空对象通常表示该 action 不受支持，而非成功的空操作。
    fn build_action(&self, action: &Action) -> Value;

    fn build_send_message(
        &self,
        target_type: &MessageType,
        target_id: i64,
        content: &str,
    ) -> Value {
        let action = Action::SendMessage {
            target: match target_type {
                MessageType::Private => ActionTarget::Private { user_id: target_id },
                MessageType::Group => ActionTarget::Group {
                    group_id: target_id,
                },
                MessageType::Channel => ActionTarget::Channel {
                    guild_id: 0,
                    channel_id: target_id,
                },
            },
            message: vec![UnifiedSegment::Text {
                text: content.to_string(),
            }],
        };
        self.build_action(&action)
    }

    /// 将消息段编码为本平台的协议负载；每个适配器必须显式实现。
    fn segments_to_protocol(&self, segments: &[UnifiedSegment]) -> Value;
}
