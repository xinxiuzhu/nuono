use super::DomainModelError;
use super::identity::{BotIdentity, ConversationRef, IdentityKind, IdentityRef, NativeId};
use crate::message::MessageSegment;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DomainEventKind {
    Message(DomainMessage),
    Notice {
        notice_type: String,
        sub_type: Option<String>,
    },
    Request {
        request_type: String,
        sub_type: Option<String>,
    },
    Meta {
        event_type: String,
        sub_type: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainMessage {
    pub message_id: NativeId,
    pub segments: Vec<MessageSegment>,
    pub sender: IdentityRef,
    #[serde(default)]
    pub raw: Value,
}

impl DomainMessage {
    pub fn new(
        message_id: impl Into<String>,
        segments: Vec<MessageSegment>,
        sender: IdentityRef,
        raw: Value,
    ) -> Result<Self, DomainModelError> {
        if *sender.kind() != IdentityKind::User {
            return Err(DomainModelError::InvalidValue {
                field: "sender",
                expected: "a user identity",
            });
        }
        Ok(Self {
            message_id: NativeId::new(message_id)?,
            segments,
            sender,
            raw,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DomainEvent {
    pub event_id: NativeId,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub bot: BotIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<IdentityRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationRef>,
    pub kind: DomainEventKind,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Deserialize)]
struct DomainEventWire {
    event_id: NativeId,
    occurred_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
    bot: BotIdentity,
    #[serde(default)]
    actor: Option<IdentityRef>,
    #[serde(default)]
    conversation: Option<ConversationRef>,
    kind: DomainEventKind,
    #[serde(default)]
    raw: Value,
}

impl DomainEvent {
    #[expect(
        clippy::too_many_arguments,
        reason = "领域事件各必填字段需要一次校验构造"
    )]
    pub fn new(
        event_id: impl Into<String>,
        occurred_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        bot: BotIdentity,
        actor: Option<IdentityRef>,
        conversation: Option<ConversationRef>,
        kind: DomainEventKind,
        raw: Value,
    ) -> Result<Self, DomainModelError> {
        Self::from_parts(
            NativeId::new(event_id)?,
            occurred_at,
            received_at,
            bot,
            actor,
            conversation,
            kind,
            raw,
        )
    }

    #[expect(clippy::too_many_arguments, reason = "与 new 对齐的内部构造辅助函数")]
    fn from_parts(
        event_id: NativeId,
        occurred_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        bot: BotIdentity,
        actor: Option<IdentityRef>,
        conversation: Option<ConversationRef>,
        kind: DomainEventKind,
        raw: Value,
    ) -> Result<Self, DomainModelError> {
        if received_at < occurred_at {
            return Err(DomainModelError::InvalidValue {
                field: "received_at",
                expected: "a timestamp at or after occurred_at",
            });
        }
        if let Some(actor) = &actor {
            if *actor.kind() != IdentityKind::User {
                return Err(DomainModelError::InvalidValue {
                    field: "actor",
                    expected: "a user identity",
                });
            }
            if actor.platform() != bot.platform() {
                return Err(DomainModelError::PlatformMismatch { field: "actor" });
            }
        }
        if let Some(conversation) = &conversation {
            conversation.validate()?;
            if conversation.platform() != bot.platform() {
                return Err(DomainModelError::PlatformMismatch {
                    field: "conversation",
                });
            }
        }

        if let DomainEventKind::Message(message) = &kind {
            let actor = actor.as_ref().ok_or(DomainModelError::InvalidValue {
                field: "actor",
                expected: "a message actor",
            })?;
            if conversation.is_none() {
                return Err(DomainModelError::InvalidValue {
                    field: "conversation",
                    expected: "a message conversation",
                });
            }
            if &message.sender != actor {
                return Err(DomainModelError::InvalidValue {
                    field: "message.sender",
                    expected: "the same identity as actor",
                });
            }
        }

        Ok(Self {
            event_id,
            occurred_at,
            received_at,
            bot,
            actor,
            conversation,
            kind,
            raw,
        })
    }

    pub fn message(&self) -> Option<&DomainMessage> {
        match &self.kind {
            DomainEventKind::Message(message) => Some(message),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for DomainEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DomainEventWire::deserialize(deserializer)?;
        Self::from_parts(
            wire.event_id,
            wire.occurred_at,
            wire.received_at,
            wire.bot,
            wire.actor,
            wire.conversation,
            wire.kind,
            wire.raw,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionScope {
    Conversation {
        conversation: ConversationRef,
    },
    Actor {
        actor: IdentityRef,
    },
    ActorInConversation {
        actor: IdentityRef,
        conversation: ConversationRef,
    },
    BotInstance,
}

impl SessionScope {
    fn validate(&self, bot: &BotIdentity) -> Result<(), DomainModelError> {
        match self {
            Self::Conversation { conversation } => {
                conversation.validate()?;
                ensure_platform(bot, conversation.platform(), "conversation")
            }
            Self::Actor { actor } => {
                ensure_actor(actor)?;
                ensure_platform(bot, actor.platform(), "actor")
            }
            Self::ActorInConversation {
                actor,
                conversation,
            } => {
                ensure_actor(actor)?;
                conversation.validate()?;
                ensure_platform(bot, actor.platform(), "actor")?;
                ensure_platform(bot, conversation.platform(), "conversation")
            }
            Self::BotInstance => Ok(()),
        }
    }
}

fn ensure_actor(actor: &IdentityRef) -> Result<(), DomainModelError> {
    if *actor.kind() != IdentityKind::User {
        return Err(DomainModelError::InvalidValue {
            field: "actor",
            expected: "a user identity",
        });
    }
    Ok(())
}

fn ensure_platform(
    bot: &BotIdentity,
    platform: &super::identity::Platform,
    field: &'static str,
) -> Result<(), DomainModelError> {
    if bot.platform() != platform {
        return Err(DomainModelError::PlatformMismatch { field });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SessionKey {
    pub version: u8,
    pub bot: BotIdentity,
    pub scope: SessionScope,
}

#[derive(Deserialize)]
struct SessionKeyWire {
    version: u8,
    bot: BotIdentity,
    scope: SessionScope,
}

impl SessionKey {
    pub const VERSION: u8 = 1;

    pub fn new(bot: BotIdentity, scope: SessionScope) -> Result<Self, DomainModelError> {
        scope.validate(&bot)?;
        Ok(Self {
            version: Self::VERSION,
            bot,
            scope,
        })
    }
}

impl<'de> Deserialize<'de> for SessionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SessionKeyWire::deserialize(deserializer)?;
        if wire.version != Self::VERSION {
            return Err(serde::de::Error::custom("unsupported session key version"));
        }
        wire.scope
            .validate(&wire.bot)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            version: wire.version,
            bot: wire.bot,
            scope: wire.scope,
        })
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        f.write_str(&encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{IdentityScheme, Platform};

    fn identity(platform: Platform, kind: IdentityKind, value: &str) -> IdentityRef {
        let scheme = if platform == Platform::Discord {
            IdentityScheme::Snowflake
        } else {
            IdentityScheme::Numeric
        };
        IdentityRef::new(platform, kind, scheme, value, None::<String>).unwrap()
    }

    fn bot(instance_id: &str, platform: Platform) -> BotIdentity {
        let account = identity(platform, IdentityKind::Bot, "1");
        BotIdentity::new(instance_id, account, None).unwrap()
    }

    #[test]
    fn session_scopes_are_stable_and_do_not_collide() {
        let platform = Platform::Telegram;
        let bot = bot("instance", platform.clone());
        let actor = identity(platform.clone(), IdentityKind::User, "42");
        let group =
            ConversationRef::group(identity(platform, IdentityKind::Group, "-1001234567890"))
                .unwrap();

        let actor_key = SessionKey::new(
            bot.clone(),
            SessionScope::Actor {
                actor: actor.clone(),
            },
        )
        .unwrap();
        let conversation_key = SessionKey::new(
            bot.clone(),
            SessionScope::Conversation {
                conversation: group.clone(),
            },
        )
        .unwrap();
        let combined_key = SessionKey::new(
            bot,
            SessionScope::ActorInConversation {
                actor,
                conversation: group,
            },
        )
        .unwrap();

        assert_ne!(actor_key.to_string(), conversation_key.to_string());
        assert_ne!(actor_key.to_string(), combined_key.to_string());
        let restored: SessionKey = serde_json::from_str(&combined_key.to_string()).unwrap();
        assert_eq!(restored, combined_key);
    }

    #[test]
    fn domain_event_roundtrips_and_rejects_sender_mismatch() {
        let platform = Platform::Telegram;
        let bot = bot("instance", platform.clone());
        let actor = identity(platform.clone(), IdentityKind::User, "42");
        let conversation = ConversationRef::direct(actor.clone(), None).unwrap();
        let message = DomainMessage::new(
            "message-1",
            vec![MessageSegment::text("hello")],
            actor.clone(),
            serde_json::json!({"update_id": 1}),
        )
        .unwrap();
        let now = Utc::now();
        let event = DomainEvent::new(
            "event-1",
            now,
            now,
            bot.clone(),
            Some(actor.clone()),
            Some(conversation.clone()),
            DomainEventKind::Message(message),
            serde_json::json!({"raw": true}),
        )
        .unwrap();
        let restored: DomainEvent =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(restored, event);

        let other = identity(platform, IdentityKind::User, "43");
        let mismatched = DomainMessage::new("message-2", vec![], other, Value::Null).unwrap();
        assert!(
            DomainEvent::new(
                "event-2",
                now,
                now,
                bot,
                Some(actor),
                Some(conversation),
                DomainEventKind::Message(mismatched),
                Value::Null,
            )
            .is_err()
        );
    }

    #[test]
    fn domain_event_rejects_invalid_wire_invariants() {
        let platform = Platform::Telegram;
        let bot = bot("instance", platform.clone());
        let actor = identity(platform, IdentityKind::User, "42");
        let conversation = ConversationRef::direct(actor.clone(), None).unwrap();
        let now = Utc::now();
        let mut value = serde_json::to_value(
            DomainEvent::new(
                "event-1",
                now,
                now,
                bot,
                Some(actor.clone()),
                Some(conversation),
                DomainEventKind::Message(
                    DomainMessage::new("message-1", vec![], actor, Value::Null).unwrap(),
                ),
                Value::Null,
            )
            .unwrap(),
        )
        .unwrap();
        value["actor"] = Value::Null;
        assert!(serde_json::from_value::<DomainEvent>(value).is_err());
    }
}
