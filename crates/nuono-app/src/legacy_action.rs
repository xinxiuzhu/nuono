//! 领域层出站动作（`OutboundAction`）与 OneBot 协议动作模型（`platform::Action`）之间的双向转换。

use crate::platform::{Action as LegacyAction, ActionTarget as LegacyActionTarget, UnifiedSegment};
pub use nuono_domain::action::*;
use nuono_domain::{
    ConversationRef, FileSource, IdentityKind, IdentityRef, IdentityScheme, MessageSegment,
    NativeId, Platform,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LegacyProjectionError {
    NonNumericNativeId {
        value: String,
    },
    NativeIdOverflow {
        value: String,
    },
    AmbiguousPlatformTarget {
        target: Box<DeliveryTarget>,
    },
    WrongPlatform {
        expected: Platform,
        actual: Platform,
    },
    InvalidTarget {
        reason: String,
    },
    UnsupportedAction {
        action: String,
    },
    InvalidLegacyDuration {
        duration_ms: i64,
    },
}

impl fmt::Display for LegacyProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonNumericNativeId { value } => {
                write!(f, "native ID is not a decimal integer: {value}")
            }
            Self::NativeIdOverflow { value } => write!(f, "native ID does not fit i64: {value}"),
            Self::AmbiguousPlatformTarget { .. } => {
                f.write_str("target cannot be projected unambiguously")
            }
            Self::WrongPlatform { expected, actual } => {
                write!(f, "expected {expected} target, got {actual}")
            }
            Self::InvalidTarget { reason } => write!(f, "invalid target: {reason}"),
            Self::UnsupportedAction { action } => write!(f, "unsupported legacy action: {action}"),
            Self::InvalidLegacyDuration { duration_ms } => {
                write!(
                    f,
                    "duration cannot be represented as legacy seconds: {duration_ms} ms"
                )
            }
        }
    }
}

impl std::error::Error for LegacyProjectionError {}

fn legacy_identity(id: i64, kind: IdentityKind) -> IdentityRef {
    IdentityRef::new(
        Platform::OneBot,
        kind,
        IdentityScheme::Numeric,
        id.to_string(),
        None::<String>,
    )
    .expect("an i64 decimal is always a valid OneBot identity")
}

fn legacy_actor(id: i64) -> IdentityRef {
    legacy_identity(id, IdentityKind::User)
}

fn legacy_group(id: i64) -> ConversationRef {
    ConversationRef::group(legacy_identity(id, IdentityKind::Group))
        .expect("the identity has group kind")
}

fn parse_legacy_id(value: &NativeId) -> Result<i64, LegacyProjectionError> {
    let numeric =
        value
            .as_str()
            .parse::<i128>()
            .map_err(|_| LegacyProjectionError::NonNumericNativeId {
                value: value.to_string(),
            })?;
    i64::try_from(numeric).map_err(|_| LegacyProjectionError::NativeIdOverflow {
        value: value.to_string(),
    })
}

fn ensure_onebot_platform(platform: &Platform) -> Result<(), LegacyProjectionError> {
    if *platform != Platform::OneBot {
        return Err(LegacyProjectionError::WrongPlatform {
            expected: Platform::OneBot,
            actual: platform.clone(),
        });
    }
    Ok(())
}

fn project_actor(actor: &IdentityRef) -> Result<i64, LegacyProjectionError> {
    ensure_onebot_platform(actor.platform())?;
    if *actor.kind() != IdentityKind::User {
        return Err(LegacyProjectionError::InvalidTarget {
            reason: "actor target must contain a user identity".to_string(),
        });
    }
    parse_legacy_id(actor.value())
}

fn project_group(conversation: &ConversationRef) -> Result<i64, LegacyProjectionError> {
    ensure_onebot_platform(conversation.platform())?;
    match conversation {
        ConversationRef::Group { group } => parse_legacy_id(group.value()),
        _ => Err(LegacyProjectionError::AmbiguousPlatformTarget {
            target: Box::new(DeliveryTarget::Conversation {
                conversation: conversation.clone(),
            }),
        }),
    }
}

impl TryFrom<LegacyActionTarget> for DeliveryTarget {
    type Error = LegacyProjectionError;

    fn try_from(target: LegacyActionTarget) -> Result<Self, Self::Error> {
        Ok(match target {
            LegacyActionTarget::Private { user_id } => Self::Actor {
                actor: legacy_actor(user_id),
            },
            LegacyActionTarget::Group { group_id } => Self::Conversation {
                conversation: legacy_group(group_id),
            },
            LegacyActionTarget::Channel {
                guild_id,
                channel_id,
            } => Self::Channel {
                channel: legacy_identity(channel_id, IdentityKind::Channel),
                parent: Some(legacy_identity(guild_id, IdentityKind::Group)),
            },
        })
    }
}

impl TryFrom<DeliveryTarget> for LegacyActionTarget {
    type Error = LegacyProjectionError;

    fn try_from(target: DeliveryTarget) -> Result<Self, Self::Error> {
        match target {
            DeliveryTarget::Actor { actor } => Ok(Self::Private {
                user_id: project_actor(&actor)?,
            }),
            DeliveryTarget::Conversation { conversation } => Ok(Self::Group {
                group_id: project_group(&conversation)?,
            }),
            DeliveryTarget::Channel { channel, parent } => {
                ensure_onebot_platform(channel.platform())?;
                if *channel.kind() != IdentityKind::Channel {
                    return Err(LegacyProjectionError::InvalidTarget {
                        reason: "channel target must contain a channel identity".to_string(),
                    });
                }
                let parent =
                    parent.ok_or_else(|| LegacyProjectionError::AmbiguousPlatformTarget {
                        target: Box::new(DeliveryTarget::Channel {
                            channel: channel.clone(),
                            parent: None,
                        }),
                    })?;
                Ok(Self::Channel {
                    guild_id: parse_legacy_id(parent.value())?,
                    channel_id: parse_legacy_id(channel.value())?,
                })
            }
            _ => Err(LegacyProjectionError::InvalidTarget {
                reason: "unsupported delivery target".to_string(),
            }),
        }
    }
}

fn from_legacy_segment(segment: UnifiedSegment) -> MessageSegment {
    match segment {
        UnifiedSegment::Text { text } => MessageSegment::Text { text },
        UnifiedSegment::Image { url } => MessageSegment::Image {
            source: FileSource::from_wire(url),
            id: None,
        },
        UnifiedSegment::At { user_id } => MessageSegment::At {
            user_id: user_id.to_string(),
        },
        UnifiedSegment::AtUser { user_id } => MessageSegment::At { user_id },
        UnifiedSegment::AtAll => MessageSegment::Unknown {
            kind: "at_all".to_string(),
            data: Value::Null,
        },
        UnifiedSegment::Face { id } => MessageSegment::Unknown {
            kind: "face".to_string(),
            data: json!({ "id": id }),
        },
        UnifiedSegment::Reply { message_id } => MessageSegment::Reply { message_id },
        UnifiedSegment::File { url, name } => MessageSegment::File {
            name,
            source: FileSource::from_wire(url),
        },
        UnifiedSegment::Voice { url } => MessageSegment::Record {
            source: FileSource::from_wire(url),
        },
        UnifiedSegment::Video { url } => MessageSegment::Video {
            source: FileSource::from_wire(url),
        },
        UnifiedSegment::Custom { type_name, data } => MessageSegment::Unknown {
            kind: type_name,
            data,
        },
    }
}

fn to_legacy_segment(segment: MessageSegment) -> Result<UnifiedSegment, LegacyProjectionError> {
    Ok(match segment {
        MessageSegment::Text { text } => UnifiedSegment::Text { text },
        MessageSegment::Image { source, .. } => UnifiedSegment::Image {
            url: source.as_str().to_owned(),
        },
        MessageSegment::At { user_id } => {
            let id =
                NativeId::new(user_id).map_err(|error| LegacyProjectionError::InvalidTarget {
                    reason: error.to_string(),
                })?;
            // 仅当用户 ID 可无损还原为相同的十进制 `i64` 字符串（无前导零或溢出）时，才使用旧版数字型 `At` 消息段。
            match id.as_str().parse::<i64>() {
                Ok(value) if value.to_string() == id.as_str() => {
                    UnifiedSegment::At { user_id: value }
                }
                _ => UnifiedSegment::AtUser {
                    user_id: id.to_string(),
                },
            }
        }
        MessageSegment::Reply { message_id } => UnifiedSegment::Reply { message_id },
        MessageSegment::File { name, source } => UnifiedSegment::File {
            url: source.as_str().to_owned(),
            name,
        },
        MessageSegment::Record { source } => UnifiedSegment::Voice {
            url: source.as_str().to_owned(),
        },
        MessageSegment::Video { source } => UnifiedSegment::Video {
            url: source.as_str().to_owned(),
        },
        MessageSegment::Unknown { kind, data } => UnifiedSegment::Custom {
            type_name: kind,
            data,
        },
        other => UnifiedSegment::Custom {
            type_name: "structured_segment".to_string(),
            data: serde_json::to_value(other).map_err(|error| {
                LegacyProjectionError::UnsupportedAction {
                    action: error.to_string(),
                }
            })?,
        },
    })
}

fn actor_target(id: i64) -> DeliveryTarget {
    DeliveryTarget::Actor {
        actor: legacy_actor(id),
    }
}

fn conversation_target(id: i64) -> DeliveryTarget {
    DeliveryTarget::Conversation {
        conversation: legacy_group(id),
    }
}

impl TryFrom<LegacyAction> for OutboundAction {
    type Error = LegacyProjectionError;

    fn try_from(action: LegacyAction) -> Result<Self, Self::Error> {
        Ok(match action {
            LegacyAction::SendMessage { target, message } => {
                let mut segments: Vec<_> = message.into_iter().map(from_legacy_segment).collect();
                let reply_to = match segments.first() {
                    Some(MessageSegment::Reply { message_id }) => {
                        let message_id = NativeId::new(message_id.clone()).map_err(|error| {
                            LegacyProjectionError::InvalidTarget {
                                reason: error.to_string(),
                            }
                        })?;
                        segments.remove(0);
                        Some(message_id)
                    }
                    _ => None,
                };
                Self::SendMessage {
                    target: target.try_into()?,
                    segments,
                    reply_to,
                }
            }
            LegacyAction::DeleteMessage { message_id } => Self::DeleteMessage {
                target: None,
                message_id: NativeId::new(message_id).map_err(|error| {
                    LegacyProjectionError::InvalidTarget {
                        reason: error.to_string(),
                    }
                })?,
            },
            LegacyAction::SendLike { user_id, times } => Self::RawPlatform {
                action: "send_like".to_string(),
                params: json!({ "user_id": user_id.to_string(), "times": times }),
            },
            LegacyAction::SetGroupBan {
                group_id,
                user_id,
                duration,
            } => Self::Moderate {
                target: actor_target(user_id),
                operation: ModerationOperation::Ban {
                    conversation: legacy_group(group_id),
                    duration_ms: i64::from(duration) * 1000,
                },
            },
            LegacyAction::SetGroupKick {
                group_id,
                user_id,
                reject_add_request,
            } => Self::Moderate {
                target: actor_target(user_id),
                operation: ModerationOperation::Kick {
                    conversation: legacy_group(group_id),
                    reject_add_request,
                },
            },
            LegacyAction::SetGroupCard {
                group_id,
                user_id,
                card,
            } => Self::Moderate {
                target: actor_target(user_id),
                operation: ModerationOperation::SetCard {
                    conversation: legacy_group(group_id),
                    card,
                },
            },
            LegacyAction::SetGroupLeave {
                group_id,
                is_dismiss,
            } => Self::Moderate {
                target: conversation_target(group_id),
                operation: ModerationOperation::Leave {
                    dismiss: is_dismiss,
                },
            },
            LegacyAction::GetGroupList => Self::Query {
                query: QueryOperation::GroupList,
            },
            LegacyAction::GetFriendList => Self::Query {
                query: QueryOperation::FriendList,
            },
            LegacyAction::GetGroupMemberInfo { group_id, user_id } => Self::Query {
                query: QueryOperation::GroupMember {
                    conversation: Box::new(legacy_group(group_id)),
                    actor: legacy_actor(user_id),
                },
            },
            LegacyAction::GetLoginInfo => Self::Query {
                query: QueryOperation::LoginInfo,
            },
            LegacyAction::SetGroupAdmin {
                group_id,
                user_id,
                enable,
            } => Self::Moderate {
                target: actor_target(user_id),
                operation: ModerationOperation::SetAdmin {
                    conversation: legacy_group(group_id),
                    enable,
                },
            },
            LegacyAction::HandleRequest {
                request_type,
                flag,
                approve,
                remark,
            } => Self::Moderate {
                target: DeliveryTarget::Actor {
                    actor: IdentityRef::new(
                        Platform::OneBot,
                        IdentityKind::User,
                        IdentityScheme::Native,
                        flag,
                        None::<String>,
                    )
                    .map_err(|error| LegacyProjectionError::InvalidTarget {
                        reason: error.to_string(),
                    })?,
                },
                operation: ModerationOperation::HandleRequest {
                    request_type,
                    approve,
                    remark,
                },
            },
            LegacyAction::Custom { action, params } => Self::RawPlatform { action, params },
        })
    }
}

fn projected_target_actor(target: &DeliveryTarget) -> Result<i64, LegacyProjectionError> {
    match target {
        DeliveryTarget::Actor { actor } => project_actor(actor),
        _ => Err(LegacyProjectionError::InvalidTarget {
            reason: "moderation operation requires an actor target".to_string(),
        }),
    }
}

impl TryFrom<OutboundAction> for LegacyAction {
    type Error = LegacyProjectionError;

    fn try_from(action: OutboundAction) -> Result<Self, Self::Error> {
        Ok(match action {
            OutboundAction::SendFile { .. } => {
                return Err(LegacyProjectionError::UnsupportedAction {
                    action: "send_file".to_string(),
                });
            }
            OutboundAction::SendMessage {
                target,
                segments,
                reply_to,
            } => {
                let mut message =
                    Vec::with_capacity(segments.len() + usize::from(reply_to.is_some()));
                if let Some(reply_to) = reply_to {
                    message.push(UnifiedSegment::Reply {
                        message_id: reply_to.into_inner(),
                    });
                }
                message.extend(
                    segments
                        .into_iter()
                        .map(to_legacy_segment)
                        .collect::<Result<Vec<_>, _>>()?,
                );
                Self::SendMessage {
                    target: target.try_into()?,
                    message,
                }
            }
            OutboundAction::DeleteMessage { message_id, .. } => Self::DeleteMessage {
                message_id: message_id.into_inner(),
            },
            OutboundAction::Moderate { target, operation } => match operation {
                ModerationOperation::Ban {
                    conversation,
                    duration_ms,
                } => {
                    let seconds = duration_ms
                        .checked_div(1000)
                        .filter(|_| duration_ms % 1000 == 0);
                    let duration = seconds
                        .and_then(|value| i32::try_from(value).ok())
                        .ok_or(LegacyProjectionError::InvalidLegacyDuration { duration_ms })?;
                    Self::SetGroupBan {
                        group_id: project_group(&conversation)?,
                        user_id: projected_target_actor(&target)?,
                        duration,
                    }
                }
                ModerationOperation::Kick {
                    conversation,
                    reject_add_request,
                } => Self::SetGroupKick {
                    group_id: project_group(&conversation)?,
                    user_id: projected_target_actor(&target)?,
                    reject_add_request,
                },
                ModerationOperation::SetCard { conversation, card } => Self::SetGroupCard {
                    group_id: project_group(&conversation)?,
                    user_id: projected_target_actor(&target)?,
                    card,
                },
                ModerationOperation::Leave { dismiss } => {
                    let DeliveryTarget::Conversation { conversation } = target else {
                        return Err(LegacyProjectionError::InvalidTarget {
                            reason: "leave requires a conversation target".to_string(),
                        });
                    };
                    Self::SetGroupLeave {
                        group_id: project_group(&conversation)?,
                        is_dismiss: dismiss,
                    }
                }
                ModerationOperation::SetAdmin {
                    conversation,
                    enable,
                } => Self::SetGroupAdmin {
                    group_id: project_group(&conversation)?,
                    user_id: projected_target_actor(&target)?,
                    enable,
                },
                ModerationOperation::HandleRequest {
                    request_type,
                    approve,
                    remark,
                } => {
                    let DeliveryTarget::Actor { actor } = target else {
                        return Err(LegacyProjectionError::InvalidTarget {
                            reason: "request handling requires an actor target".to_string(),
                        });
                    };
                    Self::HandleRequest {
                        request_type,
                        flag: actor.value().as_str().to_owned(),
                        approve,
                        remark,
                    }
                }
                _ => {
                    return Err(LegacyProjectionError::UnsupportedAction {
                        action: "moderate".to_string(),
                    });
                }
            },
            OutboundAction::Query { query } => match query {
                QueryOperation::GroupList => Self::GetGroupList,
                QueryOperation::FriendList => Self::GetFriendList,
                QueryOperation::GroupMember {
                    conversation,
                    actor,
                } => Self::GetGroupMemberInfo {
                    group_id: project_group(&conversation)?,
                    user_id: project_actor(&actor)?,
                },
                QueryOperation::LoginInfo => Self::GetLoginInfo,
                QueryOperation::Raw { name, params } => Self::Custom {
                    action: name,
                    params,
                },
                _ => {
                    return Err(LegacyProjectionError::UnsupportedAction {
                        action: "query".to_string(),
                    });
                }
            },
            OutboundAction::RawPlatform { action, params } => Self::Custom { action, params },
            OutboundAction::EditMessage { .. } => {
                return Err(LegacyProjectionError::UnsupportedAction {
                    action: "edit_message".to_string(),
                });
            }
            OutboundAction::React { .. } => {
                return Err(LegacyProjectionError::UnsupportedAction {
                    action: "react".to_string(),
                });
            }
            _ => {
                return Err(LegacyProjectionError::UnsupportedAction {
                    action: "unknown".to_string(),
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn legacy_projection_rejects_native_id_overflow() {
        let target = DeliveryTarget::actor(identity(
            Platform::OneBot,
            IdentityKind::User,
            IdentityScheme::Native,
            "9223372036854775808",
            None,
        ))
        .unwrap();
        assert!(matches!(
            LegacyActionTarget::try_from(target),
            Err(LegacyProjectionError::NativeIdOverflow { .. })
        ));
    }

    #[test]
    fn channel_without_parent_is_an_explicit_ambiguity() {
        let channel = identity(
            Platform::OneBot,
            IdentityKind::Channel,
            IdentityScheme::Native,
            "123",
            None,
        );
        assert!(matches!(
            LegacyActionTarget::try_from(DeliveryTarget::channel(channel, None).unwrap()),
            Err(LegacyProjectionError::AmbiguousPlatformTarget { .. })
        ));
    }

    #[test]
    fn ambiguity_error_is_compact_and_preserves_target_json() {
        let target = DeliveryTarget::channel(
            identity(
                Platform::OneBot,
                IdentityKind::Channel,
                IdentityScheme::Native,
                "123",
                None,
            ),
            None,
        )
        .unwrap();
        let error = LegacyActionTarget::try_from(target.clone()).unwrap_err();
        let wire = json!({
            "type": "ambiguous_platform_target",
            "target": target,
        });
        assert_eq!(serde_json::to_value(&error).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<LegacyProjectionError>(wire).unwrap(),
            error
        );
        assert!(
            std::mem::size_of::<LegacyProjectionError>() < std::mem::size_of::<DeliveryTarget>()
        );
    }

    #[test]
    fn telegram_negative_id_is_preserved_without_cross_platform_projection() {
        let target = DeliveryTarget::actor(identity(
            Platform::Telegram,
            IdentityKind::User,
            IdentityScheme::Numeric,
            "-1001234567890",
            None,
        ))
        .unwrap();
        let encoded = serde_json::to_string(&target).unwrap();
        let restored: DeliveryTarget = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            &restored,
            DeliveryTarget::Actor { actor } if actor.value().as_str() == "-1001234567890"
        ));
        assert!(matches!(
            LegacyActionTarget::try_from(restored),
            Err(LegacyProjectionError::WrongPlatform {
                expected: Platform::OneBot,
                actual: Platform::Telegram
            })
        ));
    }

    #[test]
    fn custom_maps_to_raw_platform() {
        let raw = OutboundAction::try_from(LegacyAction::Custom {
            action: "platform_method".to_string(),
            params: json!({ "native_id": "18446744073709551615" }),
        })
        .unwrap();
        assert!(matches!(
            raw,
            OutboundAction::RawPlatform { action, .. } if action == "platform_method"
        ));
    }

    #[test]
    fn native_mentions_survive_legacy_projection_and_storage() {
        for user_id in ["ou_member", "18446744073709551615", "00123", "123"] {
            let domain = MessageSegment::At {
                user_id: user_id.into(),
            };
            let legacy = to_legacy_segment(domain.clone()).unwrap();
            let restored: UnifiedSegment =
                serde_json::from_value(serde_json::to_value(&legacy).unwrap()).unwrap();
            assert_eq!(from_legacy_segment(restored), domain);
            let wire = crate::platform::outbound::compatibility_segments(&[legacy]);
            if user_id == "123" {
                assert_eq!(wire[0]["data"]["qq"], json!(123));
            } else {
                assert_eq!(wire[0]["data"]["qq"], json!(user_id));
            }
        }
    }
}
