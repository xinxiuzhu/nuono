//! 与具体协议无关的出站动作、平台能力声明、执行回执与错误定义。

use super::identity::{ConversationRef, IdentityKind, IdentityRef, NativeId, Platform};
use super::{DomainModelError, validate_text};
use crate::message::{FileSource, MessageSegment};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub type ActionId = NativeId;
pub type CorrelationId = NativeId;

/// 具备明确语义分类与平台原生身份标识的投递目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeliveryTarget {
    Actor {
        actor: IdentityRef,
    },
    Conversation {
        conversation: ConversationRef,
    },
    Channel {
        channel: IdentityRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<IdentityRef>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DeliveryTargetWire {
    Actor {
        actor: IdentityRef,
    },
    Conversation {
        conversation: ConversationRef,
    },
    Channel {
        channel: IdentityRef,
        #[serde(default)]
        parent: Option<IdentityRef>,
    },
}

impl DeliveryTarget {
    pub fn actor(actor: IdentityRef) -> Result<Self, DomainModelError> {
        if *actor.kind() != IdentityKind::User {
            return Err(DomainModelError::InvalidValue {
                field: "actor",
                expected: "a user identity",
            });
        }
        Ok(Self::Actor { actor })
    }

    pub fn conversation(conversation: ConversationRef) -> Result<Self, DomainModelError> {
        conversation.validate()?;
        Ok(Self::Conversation { conversation })
    }

    pub fn channel(
        channel: IdentityRef,
        parent: Option<IdentityRef>,
    ) -> Result<Self, DomainModelError> {
        let conversation = ConversationRef::channel(channel.clone(), parent.clone())?;
        let ConversationRef::Channel { .. } = conversation else {
            unreachable!("channel constructor always returns channel")
        };
        Ok(Self::Channel { channel, parent })
    }

    pub fn platform(&self) -> &Platform {
        match self {
            Self::Actor { actor } => actor.platform(),
            Self::Conversation { conversation } => conversation.platform(),
            Self::Channel { channel, .. } => channel.platform(),
        }
    }

    /// 依次返回投递目标中包含的所有身份引用（包括可选的私聊会话或父级频道身份）。
    /// 主目标（参与者或子频道）排在前面，关联的上下文身份排在后面，确保校验时双方都不会被遗漏。
    pub fn identities(&self) -> impl Iterator<Item = &IdentityRef> {
        let (primary, context) = match self {
            Self::Actor { actor } => (actor, None),
            Self::Conversation { conversation } => match conversation {
                ConversationRef::Direct {
                    participant,
                    conversation,
                } => (participant, conversation.as_ref()),
                ConversationRef::Group { group } => (group, None),
                ConversationRef::Channel { channel, parent } => (channel, parent.as_ref()),
            },
            Self::Channel { channel, parent } => (channel, parent.as_ref()),
        };
        std::iter::once(primary).chain(context)
    }

    /// 校验直接构造的枚举实例或反序列化得到的投递目标。
    pub fn validate(&self) -> Result<(), DomainModelError> {
        match self {
            Self::Actor { actor } => Self::actor(actor.clone()).map(|_| ()),
            Self::Conversation { conversation } => conversation.validate(),
            Self::Channel { channel, parent } => {
                Self::channel(channel.clone(), parent.clone()).map(|_| ())
            }
        }
    }
}

impl<'de> Deserialize<'de> for DeliveryTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DeliveryTargetWire::deserialize(deserializer)?;
        match wire {
            DeliveryTargetWire::Actor { actor } => Self::actor(actor),
            DeliveryTargetWire::Conversation { conversation } => Self::conversation(conversation),
            DeliveryTargetWire::Channel { channel, parent } => Self::channel(channel, parent),
        }
        .map_err(serde::de::Error::custom)
    }
}

/// 与具体平台协议无关的统一出站动作。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutboundAction {
    /// 带回执的通用文件发送动作。应用层负责校验文件载荷大小上限，
    /// 各平台适配器负责具体的上传方式与接收方参数拼装。
    SendFile {
        target: DeliveryTarget,
        source: FileSource,
        file_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    SendMessage {
        target: DeliveryTarget,
        segments: Vec<MessageSegment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<NativeId>,
    },
    EditMessage {
        target: DeliveryTarget,
        message_id: NativeId,
        segments: Vec<MessageSegment>,
    },
    DeleteMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<DeliveryTarget>,
        message_id: NativeId,
    },
    React {
        target: DeliveryTarget,
        message_id: NativeId,
        reaction: String,
    },
    Moderate {
        target: DeliveryTarget,
        operation: ModerationOperation,
    },
    Query {
        query: QueryOperation,
    },
    RawPlatform {
        action: String,
        params: Value,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum UncheckedOutboundAction {
    SendFile {
        target: DeliveryTarget,
        source: FileSource,
        file_name: String,
        #[serde(default)]
        content_type: Option<String>,
    },
    SendMessage {
        target: DeliveryTarget,
        segments: Vec<MessageSegment>,
        #[serde(default)]
        reply_to: Option<NativeId>,
    },
    EditMessage {
        target: DeliveryTarget,
        message_id: NativeId,
        segments: Vec<MessageSegment>,
    },
    DeleteMessage {
        #[serde(default)]
        target: Option<DeliveryTarget>,
        message_id: NativeId,
    },
    React {
        target: DeliveryTarget,
        message_id: NativeId,
        reaction: String,
    },
    Moderate {
        target: DeliveryTarget,
        operation: ModerationOperation,
    },
    Query {
        query: QueryOperation,
    },
    RawPlatform {
        action: String,
        params: Value,
    },
}

impl From<UncheckedOutboundAction> for OutboundAction {
    fn from(action: UncheckedOutboundAction) -> Self {
        match action {
            UncheckedOutboundAction::SendFile {
                target,
                source,
                file_name,
                content_type,
            } => Self::SendFile {
                target,
                source,
                file_name,
                content_type,
            },
            UncheckedOutboundAction::SendMessage {
                target,
                segments,
                reply_to,
            } => Self::SendMessage {
                target,
                segments,
                reply_to,
            },
            UncheckedOutboundAction::EditMessage {
                target,
                message_id,
                segments,
            } => Self::EditMessage {
                target,
                message_id,
                segments,
            },
            UncheckedOutboundAction::DeleteMessage { target, message_id } => {
                Self::DeleteMessage { target, message_id }
            }
            UncheckedOutboundAction::React {
                target,
                message_id,
                reaction,
            } => Self::React {
                target,
                message_id,
                reaction,
            },
            UncheckedOutboundAction::Moderate { target, operation } => {
                Self::Moderate { target, operation }
            }
            UncheckedOutboundAction::Query { query } => Self::Query { query },
            UncheckedOutboundAction::RawPlatform { action, params } => {
                Self::RawPlatform { action, params }
            }
        }
    }
}

impl OutboundAction {
    pub fn validate(&self) -> Result<(), DomainModelError> {
        match self {
            Self::SendFile {
                target, file_name, ..
            } => {
                target.validate()?;
                validate_text(file_name, "file_name")
            }
            Self::SendMessage { segments, .. } | Self::EditMessage { segments, .. }
                if segments.is_empty() =>
            {
                Err(DomainModelError::InvalidValue {
                    field: "segments",
                    expected: "at least one message segment",
                })
            }
            Self::React { reaction, .. } => validate_text(reaction, "reaction"),
            Self::Moderate { target, operation } => {
                let conversation = match operation {
                    ModerationOperation::Ban { conversation, .. }
                    | ModerationOperation::Kick { conversation, .. }
                    | ModerationOperation::SetCard { conversation, .. }
                    | ModerationOperation::SetAdmin { conversation, .. } => Some(conversation),
                    ModerationOperation::Leave { .. }
                    | ModerationOperation::HandleRequest { .. } => None,
                };
                if let Some(conversation) = conversation {
                    conversation.validate()?;
                    if target.platform() != conversation.platform() {
                        return Err(DomainModelError::PlatformMismatch {
                            field: "operation.conversation",
                        });
                    }
                }
                Ok(())
            }
            Self::Query {
                query:
                    QueryOperation::GroupMember {
                        conversation,
                        actor,
                    },
            } => {
                conversation.validate()?;
                if *actor.kind() != IdentityKind::User {
                    return Err(DomainModelError::InvalidValue {
                        field: "actor",
                        expected: "a user identity",
                    });
                }
                if actor.platform() != conversation.platform() {
                    return Err(DomainModelError::PlatformMismatch { field: "actor" });
                }
                Ok(())
            }
            Self::RawPlatform { action, .. } => validate_text(action, "action"),
            _ => Ok(()),
        }
    }
}

impl<'de> Deserialize<'de> for OutboundAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let action = Self::from(UncheckedOutboundAction::deserialize(deserializer)?);
        action.validate().map_err(serde::de::Error::custom)?;
        Ok(action)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModerationOperation {
    Ban {
        conversation: ConversationRef,
        duration_ms: i64,
    },
    Kick {
        conversation: ConversationRef,
        reject_add_request: bool,
    },
    SetCard {
        conversation: ConversationRef,
        card: String,
    },
    Leave {
        dismiss: bool,
    },
    SetAdmin {
        conversation: ConversationRef,
        enable: bool,
    },
    HandleRequest {
        request_type: String,
        approve: bool,
        remark: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueryOperation {
    GroupList,
    FriendList,
    GroupMember {
        conversation: Box<ConversationRef>,
        actor: IdentityRef,
    },
    LoginInfo,
    Raw {
        name: String,
        params: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionReceiptStatus {
    Accepted,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub correlation_id: CorrelationId,
    pub platform: Platform,
    pub status: ActionReceiptStatus,
    #[serde(default)]
    pub message_ids: Vec<NativeId>,
    #[serde(default)]
    pub degradations: Vec<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionErrorKind {
    InvalidTarget,
    UnsupportedCapability,
    Unauthorized,
    RateLimited { retry_after_ms: u64 },
    Rejected,
    Timeout,
    TransientTransport,
    MalformedResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionError {
    pub kind: ActionErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    /// 使用 `Box` 将可选的底层协议诊断数据移出结构体栈内存，减小 `ActionError` 体积；JSON 序列化格式保持不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Box<Value>>,
}

impl ActionError {
    pub fn new(kind: ActionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            correlation_id: None,
            raw: None,
        }
    }
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ActionError {}

impl fmt::Display for ActionErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget => f.write_str("invalid target"),
            Self::UnsupportedCapability => f.write_str("unsupported capability"),
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::RateLimited { retry_after_ms } => {
                write!(f, "rate limited (retry after {retry_after_ms} ms)")
            }
            Self::Rejected => f.write_str("rejected"),
            Self::Timeout => f.write_str("timeout"),
            Self::TransientTransport => f.write_str("transient transport failure"),
            Self::MalformedResponse => f.write_str("malformed response"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    SendFile,
    SendMessage,
    EditMessage,
    DeleteMessage,
    React,
    Moderate,
    Query,
    Media,
    Markdown,
    TemplateMarkdown,
    Buttons,
    Interactions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilitySupport {
    Native,
    Degraded,
    Unsupported,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(std::collections::BTreeMap<Capability, CapabilitySupport>);

impl CapabilitySet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, capability: Capability, support: CapabilitySupport) {
        self.0.insert(capability, support);
    }

    pub fn support(&self, capability: Capability) -> CapabilitySupport {
        self.0
            .get(&capability)
            .copied()
            .unwrap_or(CapabilitySupport::Unsupported)
    }

    pub fn supports(&self, capability: Capability) -> bool {
        self.support(capability) != CapabilitySupport::Unsupported
    }

    pub fn iter(&self) -> impl Iterator<Item = (Capability, CapabilitySupport)> + '_ {
        self.0
            .iter()
            .map(|(capability, support)| (*capability, *support))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityScheme;
    use serde_json::json;

    fn identity(
        platform: Platform,
        kind: IdentityKind,
        scheme: IdentityScheme,
        value: &str,
        namespace: Option<&str>,
    ) -> IdentityRef {
        IdentityRef::new(platform, kind, scheme, value, namespace).unwrap()
    }

    #[test]
    fn large_discord_and_feishu_targets_roundtrip_without_i64() {
        let discord = DeliveryTarget::actor(identity(
            Platform::Discord,
            IdentityKind::User,
            IdentityScheme::Snowflake,
            "18446744073709551615",
            None,
        ))
        .unwrap();
        let feishu_group = identity(
            Platform::Feishu,
            IdentityKind::Group,
            IdentityScheme::ChatId,
            "oc_9bcdef0123456789",
            Some("tenant-a"),
        );
        let feishu =
            DeliveryTarget::conversation(ConversationRef::group(feishu_group).unwrap()).unwrap();

        for target in [discord, feishu] {
            let encoded = serde_json::to_string(&target).unwrap();
            let decoded: DeliveryTarget = serde_json::from_str(&encoded).unwrap();
            assert_eq!(target, decoded);
        }
    }

    #[test]
    fn receipt_and_error_serde() {
        let receipt = ActionReceipt {
            correlation_id: NativeId::new("corr-1").unwrap(),
            platform: Platform::Discord,
            status: ActionReceiptStatus::Completed,
            message_ids: vec![
                NativeId::new("18446744073709551615").unwrap(),
                NativeId::new("18446744073709551614").unwrap(),
            ],
            degradations: vec!["attachment rendered as URL".to_string()],
            raw: json!({ "ok": true }),
        };
        let receipt_back: ActionReceipt =
            serde_json::from_str(&serde_json::to_string(&receipt).unwrap()).unwrap();
        assert_eq!(receipt, receipt_back);

        let mut error = ActionError::new(
            ActionErrorKind::RateLimited {
                retry_after_ms: 250,
            },
            "slow down",
        );
        let plain = json!({
            "kind": { "type": "rate_limited", "retry_after_ms": 250 },
            "message": "slow down"
        });
        assert_eq!(serde_json::to_value(&error).unwrap(), plain);
        assert_eq!(
            serde_json::from_value::<ActionError>(plain.clone()).unwrap(),
            error
        );
        assert_eq!(
            error.to_string(),
            "rate limited (retry after 250 ms): slow down"
        );

        error.correlation_id = Some(NativeId::new("00018446744073709551615").unwrap());
        let raw = json!({
            "details": [null, "18446744073709551615", 18_446_744_073_709_551_615_u64, {"reason": "限流"}],
            "retry_after_ms": 250
        });
        error.raw = Some(Box::new(raw.clone()));
        let mut detailed = plain.clone();
        detailed["correlation_id"] = json!("00018446744073709551615");
        detailed["raw"] = raw;
        assert_eq!(serde_json::to_value(&error).unwrap(), detailed);
        let error_back: ActionError =
            serde_json::from_str(&serde_json::to_string(&error).unwrap()).unwrap();
        assert_eq!(error, error_back);
        assert!(std::error::Error::source(&error).is_none());

        // 显式传入的 null 可选字段保持原有的反序列化为 None 的语义。
        let mut explicit_null = plain;
        explicit_null["correlation_id"] = Value::Null;
        explicit_null["raw"] = Value::Null;
        let decoded: ActionError = serde_json::from_value(explicit_null).unwrap();
        assert!(decoded.correlation_id.is_none() && decoded.raw.is_none());
        let mut null_payload = decoded;
        null_payload.raw = Some(Box::new(Value::Null));
        let encoded_null = serde_json::to_value(null_payload).unwrap();
        assert_eq!(encoded_null.get("raw"), Some(&Value::Null));
    }

    #[test]
    fn action_error_keeps_diagnostic_payload_out_of_inline_layout() {
        let error_bytes = std::mem::size_of::<ActionError>();
        let inline_bytes = std::mem::size_of::<(
            ActionErrorKind,
            String,
            Option<CorrelationId>,
            Option<Value>,
        )>();
        eprintln!(
            "ActionError: {error_bytes} bytes; inline diagnostic fields: {inline_bytes} bytes"
        );
        assert!(error_bytes < inline_bytes);
        assert!(
            error_bytes < 128,
            "errors must fit below the large-result threshold"
        );
    }
}
