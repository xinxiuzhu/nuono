//! 将通用入站事件 `UnifiedEvent` 归一化为领域层 `DomainEvent`（供标准 OneBot 事件及进程内测试沙箱使用，不涉及网络收包或平台连接）。

use crate::platform::normalization::{
    BotIdentity, ConversationRef, DateTime, DomainEvent, DomainModelError, EventMetadataPaths,
    IdentityKind, IdentityScheme, Utc, first_id, identity, missing, non_sentinel, nonzero_i64,
    normalize_with,
};
use crate::platform::{MessageType, UnifiedEvent};

const METADATA_PATHS: EventMetadataPaths = EventMetadataPaths {
    message_id: &[&["message_id"]],
    event_id: &[
        &["event_id"],
        &["id"],
        &["message_id"],
        &["message", "message_id"],
    ],
    occurred_at: &[&["time"], &["timestamp"]],
};

pub fn normalize_event(
    bot: &BotIdentity,
    event: &UnifiedEvent,
    received_at: DateTime<Utc>,
) -> Result<DomainEvent, DomainModelError> {
    normalize_with(bot, event, received_at, METADATA_PATHS, |message| {
        let actor_id = first_id(
            &message.raw,
            &[
                &["sender", "user_id"],
                &["user_id"],
                &["sender_id", "user_id"],
            ],
        )
        .or_else(|| non_sentinel(&message.sender.user_id))
        .or_else(|| nonzero_i64(message.user_id).map(|id| id.to_string()))
        .ok_or(missing("actor"))?;
        let actor = identity(
            bot.platform().clone(),
            IdentityKind::User,
            IdentityScheme::Native,
            actor_id,
            None,
        )?;

        let conversation = match message.message_type {
            MessageType::Private => ConversationRef::direct(actor.clone(), None)?,
            MessageType::Group => {
                let group_id = first_id(&message.raw, &[&["group_id"]])
                    .or_else(|| {
                        message
                            .group_id
                            .and_then(nonzero_i64)
                            .map(|id| id.to_string())
                    })
                    .ok_or(missing("conversation"))?;
                ConversationRef::group(identity(
                    bot.platform().clone(),
                    IdentityKind::Group,
                    IdentityScheme::Native,
                    group_id,
                    None,
                )?)?
            }
            MessageType::Channel => {
                let channel_id = first_id(&message.raw, &[&["channel_id"]])
                    .or_else(|| {
                        message
                            .group_id
                            .and_then(nonzero_i64)
                            .map(|id| id.to_string())
                    })
                    .ok_or(missing("conversation"))?;
                let parent = first_id(&message.raw, &[&["guild_id"]])
                    .map(|guild_id| {
                        identity(
                            bot.platform().clone(),
                            IdentityKind::Organization,
                            IdentityScheme::Native,
                            guild_id,
                            None,
                        )
                    })
                    .transpose()?;
                ConversationRef::channel(
                    identity(
                        bot.platform().clone(),
                        IdentityKind::Channel,
                        IdentityScheme::Native,
                        channel_id,
                        None,
                    )?,
                    parent,
                )?
            }
        };
        Ok((actor, conversation))
    })
}
