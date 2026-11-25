//! runtime 出站动作翻译与发送。
//!
//! runtime 内部统一生成 OneBot 兼容 action；本模块负责把它翻译为平台无关的
//! `OutboundAction`（目标恢复、消息段降级、查询白名单），并按 transport →
//! bot_sender → action_sink 的优先级完成实际发送。

use crate::core::model::{FileSource, MessageSegment};
use crate::core::transport::{ActionTransport, TransportAction};
use crate::domain::{
    BotIdentity, ConversationRef, DeliveryTarget, DomainEvent, IdentityKind, IdentityRef,
    IdentityScheme, NativeId, OutboundAction, Platform, QueryOperation,
};
use nuono_app::transport::TransportFrame;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

use super::event_kernel::RuntimeActionSink;

type RuntimeActionTranslationResult<T> = std::result::Result<T, String>;

pub(crate) async fn send_transport_runtime_action(
    action_transport: &dyn ActionTransport,
    bot: &BotIdentity,
    domain_event: Option<&DomainEvent>,
    action: serde_json::Value,
    stage: &str,
) -> bool {
    let outbound = match runtime_action_to_outbound(&action, domain_event) {
        Ok(action) => action,
        Err(error) => {
            warn!(
                bot_id = %bot.account_id.value(),
                stage,
                action = action
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown"),
                error,
                "运行时 action 无法转换为 typed transport action"
            );
            return false;
        }
    };
    match nuono_app::action::notify_action(
        action_transport,
        TransportAction::new(bot.clone(), outbound).with_authority(
            nuono_app::action::ActionAuthority::Runtime {
                stage: stage.into(),
            },
        ),
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            warn!(
                bot_id = %bot.account_id.value(),
                stage,
                error = %error,
                "运行时 typed action 发送失败"
            );
            false
        }
    }
}

pub(crate) fn runtime_action_to_outbound(
    action: &serde_json::Value,
    domain_event: Option<&DomainEvent>,
) -> RuntimeActionTranslationResult<OutboundAction> {
    let action_name = action
        .get("action")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "action name is required".to_string())?;
    let params = action
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let params_object = params
        .as_object()
        .ok_or_else(|| format!("{action_name} params must be an object"))?;

    match action_name {
        "send_msg" | "send_private_msg" | "send_group_msg" | "send_guild_channel_msg" => {
            let target = runtime_send_target(action_name, params_object, domain_event)?;
            let (mut segments, segment_reply_to) = runtime_message_segments(
                params_object
                    .get("message")
                    .ok_or_else(|| format!("{action_name} requires message"))?,
            )?;
            if segments.is_empty() {
                segments.push(MessageSegment::text(""));
            }
            let reply_to = segment_reply_to
                .or_else(|| {
                    runtime_optional_id(params_object, "reply_to")
                        .ok()
                        .flatten()
                })
                .or_else(|| {
                    action
                        .get("reply_current")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                        .then(|| {
                            runtime_optional_id(params_object, "message_id")
                                .ok()
                                .flatten()
                                .or_else(|| {
                                    domain_event
                                        .and_then(DomainEvent::message)
                                        .map(|message| message.message_id.clone())
                                })
                        })
                        .flatten()
                });
            Ok(OutboundAction::SendMessage {
                target,
                segments,
                reply_to,
            })
        }
        "delete_msg" | "delete_message" => Ok(OutboundAction::DeleteMessage {
            target: runtime_explicit_target(params_object, domain_event)?.or_else(|| {
                domain_event
                    .and_then(|event| event.conversation.as_ref())
                    .and_then(|conversation| delivery_target_from_conversation(conversation).ok())
            }),
            message_id: runtime_required_id(params_object, "message_id")?,
        }),
        "get_group_list" => Ok(OutboundAction::Query {
            query: QueryOperation::GroupList,
        }),
        "get_friend_list" => Ok(OutboundAction::Query {
            query: QueryOperation::FriendList,
        }),
        "get_login_info" | "get_self_info" => Ok(OutboundAction::Query {
            query: QueryOperation::LoginInfo,
        }),
        "get_group_member_info" => {
            let conversation = match runtime_optional_id(params_object, "group_id")? {
                Some(group_id) => ConversationRef::group(runtime_identity_ref(
                    domain_event,
                    IdentityKind::Group,
                    group_id,
                )?)
                .map_err(|error| error.to_string())?,
                None => match domain_event.and_then(|event| event.conversation.clone()) {
                    Some(conversation @ ConversationRef::Group { .. }) => conversation,
                    _ => return Err("get_group_member_info requires group_id".to_string()),
                },
            };
            let actor = match runtime_optional_id(params_object, "user_id")? {
                Some(user_id) => runtime_identity_ref(domain_event, IdentityKind::User, user_id)?,
                None => domain_event
                    .and_then(|event| event.actor.clone())
                    .ok_or_else(|| "get_group_member_info requires user_id".to_string())?,
            };
            Ok(OutboundAction::Query {
                query: QueryOperation::GroupMember {
                    conversation: Box::new(conversation),
                    actor,
                },
            })
        }
        name if is_supported_runtime_query(name) => Ok(OutboundAction::Query {
            query: QueryOperation::Raw {
                name: name.to_string(),
                params,
            },
        }),
        name => Ok(OutboundAction::RawPlatform {
            action: name.to_string(),
            params,
        }),
    }
}

fn runtime_send_target(
    action_name: &str,
    params: &serde_json::Map<String, serde_json::Value>,
    domain_event: Option<&DomainEvent>,
) -> RuntimeActionTranslationResult<DeliveryTarget> {
    let explicit = match action_name {
        "send_private_msg" => runtime_private_target(params, domain_event)?,
        "send_group_msg" => runtime_group_target(params, domain_event)?,
        "send_guild_channel_msg" => runtime_channel_target(params, domain_event)?,
        "send_msg" => runtime_explicit_target(params, domain_event)?,
        _ => None,
    };
    if let Some(target) = explicit {
        return Ok(target);
    }

    let event = domain_event.ok_or_else(|| format!("{action_name} has no explicit target"))?;
    match action_name {
        "send_private_msg" => event
            .actor
            .clone()
            .map(DeliveryTarget::actor)
            .transpose()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "send_private_msg requires user_id".to_string()),
        "send_group_msg" => match event.conversation.as_ref() {
            Some(conversation @ ConversationRef::Group { .. }) => {
                delivery_target_from_conversation(conversation)
            }
            _ => Err("send_group_msg requires group_id".to_string()),
        },
        "send_guild_channel_msg" => match event.conversation.as_ref() {
            Some(conversation @ ConversationRef::Channel { .. }) => {
                delivery_target_from_conversation(conversation)
            }
            _ => Err("send_guild_channel_msg requires guild_id and channel_id".to_string()),
        },
        "send_msg" => event
            .conversation
            .as_ref()
            .map(delivery_target_from_conversation)
            .transpose()?
            .or_else(|| {
                event
                    .actor
                    .clone()
                    .and_then(|actor| DeliveryTarget::actor(actor).ok())
            })
            .ok_or_else(|| "send_msg has no current conversation target".to_string()),
        _ => unreachable!("send action names are matched by caller"),
    }
}

fn runtime_explicit_target(
    params: &serde_json::Map<String, serde_json::Value>,
    domain_event: Option<&DomainEvent>,
) -> RuntimeActionTranslationResult<Option<DeliveryTarget>> {
    let message_type = params
        .get("message_type")
        .or_else(|| params.get("detail_type"))
        .and_then(serde_json::Value::as_str);
    match message_type {
        Some("private") => runtime_private_target(params, domain_event),
        Some("group") => runtime_group_target(params, domain_event),
        Some("channel") | Some("guild") => runtime_channel_target(params, domain_event),
        Some(other) => Err(format!("unsupported message_type {other}")),
        None if params.contains_key("channel_id") || params.contains_key("guild_id") => {
            runtime_channel_target(params, domain_event)
        }
        None if params.contains_key("group_id") => runtime_group_target(params, domain_event),
        None if params.contains_key("user_id") => runtime_private_target(params, domain_event),
        None => Ok(None),
    }
}

fn runtime_private_target(
    params: &serde_json::Map<String, serde_json::Value>,
    domain_event: Option<&DomainEvent>,
) -> RuntimeActionTranslationResult<Option<DeliveryTarget>> {
    runtime_optional_id(params, "user_id")?
        .map(|user_id| {
            DeliveryTarget::actor(runtime_identity_ref(
                domain_event,
                IdentityKind::User,
                user_id,
            )?)
            .map_err(|error| error.to_string())
        })
        .transpose()
}

fn runtime_group_target(
    params: &serde_json::Map<String, serde_json::Value>,
    domain_event: Option<&DomainEvent>,
) -> RuntimeActionTranslationResult<Option<DeliveryTarget>> {
    runtime_optional_id(params, "group_id")?
        .map(|group_id| {
            let group = runtime_identity_ref(domain_event, IdentityKind::Group, group_id)?;
            let conversation = ConversationRef::group(group).map_err(|error| error.to_string())?;
            DeliveryTarget::conversation(conversation).map_err(|error| error.to_string())
        })
        .transpose()
}

fn runtime_channel_target(
    params: &serde_json::Map<String, serde_json::Value>,
    domain_event: Option<&DomainEvent>,
) -> RuntimeActionTranslationResult<Option<DeliveryTarget>> {
    let guild_id = runtime_optional_id(params, "guild_id")?;
    let channel_id = runtime_optional_id(params, "channel_id")?;
    match (guild_id, channel_id) {
        (None, None) => Ok(None),
        (Some(guild_id), Some(channel_id)) => {
            let channel = runtime_identity_ref(domain_event, IdentityKind::Channel, channel_id)?;
            let guild = runtime_identity_ref(domain_event, IdentityKind::Group, guild_id)?;
            DeliveryTarget::channel(channel, Some(guild))
                .map(Some)
                .map_err(|error| error.to_string())
        }
        _ => Err("channel target requires both guild_id and channel_id".to_string()),
    }
}

fn delivery_target_from_conversation(
    conversation: &ConversationRef,
) -> RuntimeActionTranslationResult<DeliveryTarget> {
    match conversation {
        ConversationRef::Channel { channel, parent } => {
            DeliveryTarget::channel(channel.clone(), parent.clone())
                .map_err(|error| error.to_string())
        }
        _ => DeliveryTarget::conversation(conversation.clone()).map_err(|error| error.to_string()),
    }
}

fn domain_platform(domain_event: Option<&DomainEvent>) -> Platform {
    domain_event
        .map(|event| event.bot.platform().clone())
        .unwrap_or(Platform::OneBot)
}

fn runtime_identity_ref(
    event: Option<&DomainEvent>,
    kind: IdentityKind,
    value: NativeId,
) -> RuntimeActionTranslationResult<IdentityRef> {
    // 兼容层动作显式指定的目标 ID 若与当前入站事件的目标一致，则直接复用入站事件中已验证的原生身份（如保留飞书的 user_id 类型），
    // 仅当发往同平台的其他目标时才按平台命名空间重新构造身份引用。
    if let Some(event) = event {
        let matches =
            |identity: &&IdentityRef| *identity.kind() == kind && *identity.value() == value;
        if let Some(identity) = event.actor.iter().find(matches) {
            return Ok(identity.clone());
        }
        let identities: Vec<&IdentityRef> = match event.conversation.as_ref() {
            Some(ConversationRef::Direct {
                participant,
                conversation,
            }) => std::iter::once(participant)
                .chain(conversation.iter())
                .collect(),
            Some(ConversationRef::Group { group }) => vec![group],
            Some(ConversationRef::Channel { channel, parent }) => {
                std::iter::once(channel).chain(parent.iter()).collect()
            }
            Some(_) | None => Vec::new(),
        };
        if let Some(identity) = identities.into_iter().find(matches) {
            return Ok(identity.clone());
        }
    }
    let platform = domain_platform(event);
    let (scheme, namespace) = match &platform {
        Platform::Telegram => (IdentityScheme::Numeric, None),
        Platform::Discord => (IdentityScheme::Snowflake, None),
        Platform::QqOfficial => (
            IdentityScheme::OpenId,
            event.map(|event| event.bot.account_id.value().as_str()),
        ),
        Platform::Feishu => (
            if kind == IdentityKind::User {
                IdentityScheme::OpenId
            } else {
                IdentityScheme::ChatId
            },
            event.map(|event| event.bot.account_id.value().as_str()),
        ),
        _ => (IdentityScheme::Native, None),
    };
    IdentityRef::new(platform, kind, scheme, value.into_inner(), namespace)
        .map_err(|error| error.to_string())
}

fn runtime_required_id(
    params: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> RuntimeActionTranslationResult<NativeId> {
    runtime_optional_id(params, key)?.ok_or_else(|| format!("{key} is required"))
}

fn runtime_optional_id(
    params: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> RuntimeActionTranslationResult<Option<NativeId>> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let value = match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Null => return Ok(None),
        _ => return Err(format!("{key} must be a string or number")),
    };
    NativeId::new(value)
        .map(Some)
        .map_err(|error| format!("invalid {key}: {error}"))
}

fn runtime_message_segments(
    message: &serde_json::Value,
) -> RuntimeActionTranslationResult<(Vec<MessageSegment>, Option<NativeId>)> {
    let mut segments = match message {
        serde_json::Value::String(text) => vec![MessageSegment::text(text.clone())],
        serde_json::Value::Array(values) => values
            .iter()
            .map(runtime_message_segment)
            .collect::<RuntimeActionTranslationResult<Vec<_>>>()?,
        value => vec![MessageSegment::text(value.to_string())],
    };
    let reply_to = match segments.first() {
        Some(MessageSegment::Reply { message_id }) => {
            let reply_to = NativeId::new(message_id.clone())
                .map_err(|error| format!("invalid reply message_id: {error}"))?;
            segments.remove(0);
            Some(reply_to)
        }
        _ => None,
    };
    Ok((segments, reply_to))
}

fn runtime_message_segment(
    segment: &serde_json::Value,
) -> RuntimeActionTranslationResult<MessageSegment> {
    let object = segment
        .as_object()
        .ok_or_else(|| "message segment must be an object".to_string())?;
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "message segment type is required".to_string())?;
    let data = object
        .get("data")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let data_object = data
        .as_object()
        .ok_or_else(|| format!("{kind} segment data must be an object"))?;
    let string_field = |key: &str| {
        data_object.get(key).and_then(|value| match value {
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
    };

    Ok(match kind {
        "text" => MessageSegment::text(string_field("text").unwrap_or_default()),
        "image" => {
            let file = string_field("file")
                .or_else(|| string_field("file_id"))
                .ok_or_else(|| "image segment requires file or file_id".to_string())?;
            MessageSegment::Image {
                source: if data_object.contains_key("file_id") {
                    FileSource::Id(file)
                } else {
                    FileSource::from_wire(file)
                },
                id: string_field("file_id"),
            }
        }
        "at" | "mention" => {
            let user_id = string_field("qq")
                .or_else(|| string_field("user_id"))
                .ok_or_else(|| format!("{kind} segment requires user identity"))?;
            if user_id == "all" {
                MessageSegment::Unknown {
                    kind: "mention_all".to_string(),
                    data: serde_json::Value::Null,
                }
            } else {
                MessageSegment::At { user_id }
            }
        }
        "mention_all" => MessageSegment::Unknown {
            kind: "mention_all".to_string(),
            data: serde_json::Value::Null,
        },
        "reply" => MessageSegment::Reply {
            message_id: string_field("id")
                .or_else(|| string_field("message_id"))
                .ok_or_else(|| "reply segment requires message_id".to_string())?,
        },
        "file" => {
            let file = string_field("file")
                .or_else(|| string_field("file_id"))
                .ok_or_else(|| "file segment requires file or file_id".to_string())?;
            MessageSegment::File {
                name: string_field("name").unwrap_or_else(|| "file".to_string()),
                source: if data_object.contains_key("file_id") {
                    FileSource::Id(file)
                } else {
                    FileSource::from_wire(file)
                },
            }
        }
        "record" | "voice" | "audio" => {
            let file = string_field("file")
                .or_else(|| string_field("file_id"))
                .ok_or_else(|| format!("{kind} segment requires file or file_id"))?;
            MessageSegment::Record {
                source: if data_object.contains_key("file_id") {
                    FileSource::Id(file)
                } else {
                    FileSource::from_wire(file)
                },
            }
        }
        "video" => {
            let file = string_field("file")
                .or_else(|| string_field("file_id"))
                .ok_or_else(|| "video segment requires file or file_id".to_string())?;
            MessageSegment::Video {
                source: if data_object.contains_key("file_id") {
                    FileSource::Id(file)
                } else {
                    FileSource::from_wire(file)
                },
            }
        }
        _ => MessageSegment::Unknown {
            kind: kind.to_string(),
            data,
        },
    })
}

fn is_supported_runtime_query(action: &str) -> bool {
    matches!(
        action,
        "get_status"
            | "get_version"
            | "get_supported_actions"
            | "get_user_info"
            | "get_group_info"
            | "get_group_member_list"
            | "get_guild_info"
            | "get_guild_list"
            | "get_guild_member_info"
            | "get_guild_member_list"
            | "get_channel_info"
            | "get_channel_list"
            | "get_channel_member_info"
            | "get_channel_member_list"
    )
}

pub(crate) fn runtime_identity(event: &serde_json::Value, key: &str) -> Option<String> {
    match event.get(key)? {
        serde_json::Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) async fn send_runtime_action(
    bot_id: i64,
    bot_sender: Option<&mpsc::Sender<TransportFrame>>,
    action_sink: Option<&Arc<dyn RuntimeActionSink>>,
    action_transport: Option<&Arc<dyn ActionTransport>>,
    domain_event: Option<&DomainEvent>,
    action: serde_json::Value,
    stage: &str,
) -> bool {
    if let (Some(action_transport), Some(event)) = (action_transport, domain_event) {
        return send_transport_runtime_action(
            action_transport.as_ref(),
            &event.bot,
            Some(event),
            action,
            stage,
        )
        .await;
    }

    if let Some(sender) = bot_sender {
        return sender
            .send(TransportFrame::Text(action.to_string()))
            .await
            .is_ok();
    }

    let Some(action_sink) = action_sink else {
        warn!(bot_id = %bot_id, stage, "运行时事件没有平台发送通道，跳过回复发送");
        return false;
    };

    match action_sink.send_action(action).await {
        Ok(()) => true,
        Err(e) => {
            warn!(bot_id = %bot_id, stage, error = %e, "运行时事件发送失败");
            false
        }
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::domain::DomainEventKind;
    use serde_json::json;

    fn event(platform: &str, bot: &str) -> DomainEvent {
        let bot = super::super::runtime_transport::route_identity(
            &crate::core::RuntimeBotRoute::new(platform, "runtime-identity-test", bot),
        )
        .unwrap();
        let now = crate::clock::db_now();
        DomainEvent::new(
            "event",
            now,
            now,
            bot,
            None,
            None,
            DomainEventKind::Notice {
                notice_type: "fixture".into(),
                sub_type: None,
            },
            serde_json::Value::Null,
        )
        .unwrap()
    }

    #[test]
    fn explicit_runtime_targets_keep_native_scheme_and_application_namespace() {
        for (platform, bot, user, group, scheme, namespace) in [
            ("onebot", "42", "7", "9", IdentityScheme::Native, None),
            ("telegram", "42", "7", "-9", IdentityScheme::Numeric, None),
            ("discord", "42", "7", "9", IdentityScheme::Snowflake, None),
            (
                "qq",
                "app",
                "user-openid",
                "group-openid",
                IdentityScheme::OpenId,
                Some("app"),
            ),
            (
                "feishu",
                "cli_app",
                "ou_user",
                "oc_group",
                IdentityScheme::OpenId,
                Some("cli_app"),
            ),
        ] {
            let event = event(platform, bot);
            for (action, key, id, kind) in [
                ("send_private_msg", "user_id", user, IdentityKind::User),
                ("send_group_msg", "group_id", group, IdentityKind::Group),
            ] {
                let outgoing = runtime_action_to_outbound(
                    &json!({"action":action,"params":{key:id,"message":"reply"}}),
                    Some(&event),
                )
                .unwrap();
                let OutboundAction::SendMessage { target, .. } = outgoing else {
                    panic!("message target")
                };
                let identity = match &target {
                    DeliveryTarget::Actor { actor } => actor,
                    DeliveryTarget::Conversation { conversation } => conversation.identity(),
                    _ => panic!("expected actor or group"),
                };
                assert_eq!(*identity.kind(), kind);
                assert_eq!(identity.value().as_str(), id);
                assert_eq!(identity.namespace().map(NativeId::as_str), namespace);
                assert_eq!(
                    *identity.scheme(),
                    if platform == "feishu" && kind == IdentityKind::Group {
                        IdentityScheme::ChatId
                    } else {
                        scheme.clone()
                    },
                    "{platform} {action}"
                );
            }
        }
    }

    #[test]
    fn current_feishu_identity_preserves_its_verified_user_id_scheme() {
        let mut event = event("feishu", "cli_app");
        let actor = IdentityRef::new(
            Platform::Feishu,
            IdentityKind::User,
            IdentityScheme::UserId,
            "employee",
            Some("cli_app"),
        )
        .unwrap();
        event.actor = Some(actor.clone());
        let outgoing = runtime_action_to_outbound(
            &json!({"action":"send_private_msg","params":{"user_id":"employee","message":"reply"}}),
            Some(&event),
        )
        .unwrap();
        let OutboundAction::SendMessage { target, .. } = outgoing else {
            panic!("message target")
        };
        assert_eq!(target, DeliveryTarget::actor(actor).unwrap());
    }
}
