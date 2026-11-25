//! 入站消息的多模态内容提取与带版本演进的会话历史记录封装。
//!
//! 只有存储角色为内部 `user_context` 的记录才会被解码为结构化历史信封；
//! 普通 `user` 角色的消息哪怕文本恰好是合法 JSON，也绝不会被误解析为元数据。

use nuono_domain::{
    FileSource, IdentityKind, IdentityRef, IdentityScheme, MessageSegment, Platform,
};
use serde::{Deserialize, Serialize};
use validator::ValidateUrl;

use crate::ai::ChatMessage;

use super::AgentContext;

pub const USER_CONTEXT_ROLE: &str = "user_context";
const HISTORY_VERSION: u8 = 1;
const MAX_IMAGES: usize = 4;
const MAX_IMAGE_URL_BYTES: usize = 8192;

pub(super) struct MessageInput {
    pub(super) content: String,
    pub(super) images: Vec<String>,
    pub(super) reply_to_message_id: Option<String>,
}

impl MessageInput {
    pub(super) fn from_segments(segments: &[MessageSegment]) -> Self {
        let mut content = String::new();
        let mut images = Vec::new();
        let mut reply_to_message_id = None;
        let mut unavailable = false;
        let mut over_limit = false;
        for segment in segments {
            match segment {
                MessageSegment::Text { text } => content.push_str(text),
                MessageSegment::Image {
                    source: FileSource::Url(url),
                    ..
                } if usable_image_url(url) => {
                    if images.contains(url) {
                        continue;
                    }
                    if images.len() < MAX_IMAGES {
                        images.push(url.clone());
                    } else {
                        over_limit = true;
                    }
                }
                MessageSegment::Image { .. } => unavailable = true,
                MessageSegment::Reply { message_id } if reply_to_message_id.is_none() => {
                    reply_to_message_id = Some(message_id.clone());
                }
                // 忽略原始协议负载、合并转发节点或未知消息段，避免未受控的数据混入模型输入。
                _ => {}
            }
        }
        if unavailable {
            append_notice(
                &mut content,
                "[部分图片无法读取：仅支持有效的 HTTP(S) 图片链接]",
            );
        }
        if over_limit {
            append_notice(&mut content, "[超过 4 张限制的图片未读取]");
        }
        if content.trim().is_empty() {
            if !images.is_empty() {
                content = "[图片]".into();
            } else if reply_to_message_id.is_some() {
                content = "[引用消息]".into();
            }
        }
        Self {
            content,
            images,
            reply_to_message_id,
        }
    }
}

fn append_notice(content: &mut String, notice: &str) {
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(notice);
}

fn usable_image_url(url: &str) -> bool {
    url.len() <= MAX_IMAGE_URL_BYTES
        && (url
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            || url
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://")))
        && !url.chars().any(char::is_whitespace)
        && !url.chars().any(char::is_control)
        && url.split_once("://").is_some_and(|(_, rest)| {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            !authority.contains('@') && !rest.contains('\\')
        })
        && url.validate_url()
}

/// 序列化后存放在会话历史 `content` 列中的用户消息记录。
///
/// 保留完整的发言人平台身份与命名空间；外层通过数据库的 `role = "user_context"` 区分信封，`version` 字段用于后续格式演进。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryMessage {
    version: u8,
    pub speaker: IdentityRef,
    pub message_id: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(default)]
    pub shared: bool,
}

impl HistoryMessage {
    pub fn from_context(ctx: &AgentContext, content: &str) -> Self {
        Self {
            version: HISTORY_VERSION,
            speaker: speaker(ctx),
            message_id: ctx.message_id.clone(),
            content: content.to_string(),
            images: bounded_images(&ctx.images),
            reply_to_message_id: ctx.reply_to_message_id.clone(),
            shared: ctx.is_shared_conversation(),
        }
    }

    /// 解码结构化历史消息；若为旧版纯文本记录、角色不匹配或版本号不支持则返回 `None`。
    pub fn decode(role: &str, raw: &str) -> Option<Self> {
        if role != USER_CONTEXT_ROLE {
            return None;
        }
        let mut message: Self = serde_json::from_str(raw).ok()?;
        if message.version != HISTORY_VERSION || *message.speaker.kind() != IdentityKind::User {
            return None;
        }
        message.images = bounded_images(&message.images);
        Some(message)
    }

    pub fn chat_message(&self) -> ChatMessage {
        let content = if self.shared || self.reply_to_message_id.is_some() {
            format!("[消息信息: {}]\n{}", self.metadata(), self.content)
        } else {
            self.content.clone()
        };
        ChatMessage::user(content).with_images(self.images.clone())
    }

    /// 生成供上下文展示的被引用消息摘要文本（调用方可在需要时单独附加 `images`）。
    pub fn reply_excerpt(&self) -> String {
        let content: String = self.content.chars().take(4096).collect();
        format!("[引用消息信息: {}]\n{content}", self.metadata())
    }

    fn metadata(&self) -> String {
        serde_json::json!({
            "speaker": self.speaker,
            "message_id": self.message_id,
            "reply_to_message_id": self.reply_to_message_id,
        })
        .to_string()
    }
}

impl AgentContext {
    pub fn user_message(&self, clean_text: &str) -> ChatMessage {
        HistoryMessage::from_context(self, clean_text).chat_message()
    }

    /// 在群聊等多人共享会话中附带回复目标元数据，防止多个并发任务乱序完成时丢失回复归属。
    pub(super) fn assistant_history_content(&self, content: &str) -> String {
        if !self.is_shared_conversation() {
            return content.into();
        }
        let metadata = serde_json::json!({
            "in_reply_to_speaker": speaker(self),
            "in_reply_to_message_id": self.message_id,
        });
        format!("[AI 回复信息: {metadata}]\n{content}")
    }

    /// 序列化为结构化历史记录字符串（落库时必须搭配 `USER_CONTEXT_ROLE` 角色，不可使用旧版 `user` 角色）。
    pub fn history_content(&self, clean_text: &str) -> String {
        serde_json::to_string(&HistoryMessage::from_context(self, clean_text))
            .expect("conversation input serializes")
    }
}

fn bounded_images(images: &[String]) -> Vec<String> {
    let mut bounded = Vec::new();
    for image in images.iter().filter(|image| usable_image_url(image)) {
        if !bounded.contains(image) {
            bounded.push(image.clone());
            if bounded.len() == MAX_IMAGES {
                break;
            }
        }
    }
    bounded
}

fn speaker(ctx: &AgentContext) -> IdentityRef {
    if let Some(native) = &ctx.native {
        return native.actor.clone();
    }
    let platform = match ctx.platform.as_str() {
        "onebot" => Platform::OneBot,
        "telegram" => Platform::Telegram,
        "discord" => Platform::Discord,
        "feishu" => Platform::Feishu,
        "qq_official" => Platform::QqOfficial,
        other => Platform::Other(other.into()),
    };
    IdentityRef::new(
        platform,
        IdentityKind::User,
        IdentityScheme::Native,
        ctx.user_identity(),
        None::<String>,
    )
    .expect("legacy actor identity is valid")
}

pub(super) fn append_reply_context(
    message: &mut ChatMessage,
    reply_id: Option<&str>,
    history: &[(String, String)],
) {
    let Some(reply_id) = reply_id else {
        return;
    };
    let found = history
        .iter()
        .rev()
        .filter_map(|(role, content)| HistoryMessage::decode(role, content))
        .find(|entry| entry.message_id.as_deref() == Some(reply_id));
    if let Some(quoted) = found {
        message.content.push_str("\n[引用原消息，仅作背景]\n");
        message.content.push_str(&quoted.reply_excerpt());
        for image in quoted.images {
            if message.images.len() < 4 && !message.images.contains(&image) {
                message.images.push(image);
            }
        }
    } else {
        message
            .content
            .push_str("\n[引用原消息不在当前会话的近期记录中，无法读取；请勿猜测其内容。]");
    }
}

// 独立限制历史上下文中的图片总数（最多保留最近 16 张），超出的旧图片移除并在正文中追加提示标记。
pub(super) fn trim_history_images(messages: &mut [ChatMessage]) {
    let mut remaining = 16usize;
    for message in messages.iter_mut().rev() {
        if message.images.len() > remaining {
            message.images.truncate(remaining);
            message
                .content
                .push_str("\n[更早的图片已移出本轮视觉上下文]");
        }
        remaining -= message.images.len();
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nuono_domain::{BotIdentity, ConversationRef, DomainEvent, DomainEventKind, DomainMessage};
    use serde_json::json;

    use super::*;

    fn identity(kind: IdentityKind, value: &str, namespace: Option<&str>) -> IdentityRef {
        IdentityRef::new(
            Platform::OneBot,
            kind,
            IdentityScheme::Native,
            value,
            namespace,
        )
        .unwrap()
    }

    fn context(segments: Vec<MessageSegment>, group: bool) -> AgentContext {
        let actor = identity(IdentityKind::User, "member-A", Some("tenant-A"));
        let bot = BotIdentity::new(
            "instance-A",
            identity(IdentityKind::Bot, "bot-A", Some("tenant-A")),
            None,
        )
        .unwrap();
        let conversation = if group {
            ConversationRef::group(identity(IdentityKind::Group, "group-A", Some("tenant-A")))
                .unwrap()
        } else {
            ConversationRef::direct(actor.clone(), None).unwrap()
        };
        let message = DomainMessage::new("message-A", segments, actor.clone(), json!({})).unwrap();
        let now = Utc::now();
        let event = DomainEvent::new(
            "event-A",
            now,
            now,
            bot,
            Some(actor),
            Some(conversation),
            DomainEventKind::Message(message),
            json!({}),
        )
        .unwrap();
        AgentContext::from_domain_event(&event).unwrap()
    }

    fn image(source: FileSource) -> MessageSegment {
        MessageSegment::Image { source, id: None }
    }

    fn persona(id: &str) -> crate::persona::PersonaSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "name": "character",
            "description": "",
            "system_prompt": "",
            "enabled": true,
            "avatar_filename": null,
            "portrait_filename": null,
            "reference_audio_filename": null,
            "reference_text": "",
            "language": "zh",
            "tts_provider": null,
            "voice_id": null,
            "revision": 1,
            "updated_at": "2026-10-05",
        }))
        .unwrap()
    }

    #[test]
    fn group_members_share_history_but_keep_separate_active_run_keys() {
        let first = context(vec![MessageSegment::text("hello")], true);
        let mut second = first.clone();
        second.native.as_mut().unwrap().actor =
            identity(IdentityKind::User, "member-B", Some("tenant-A"));
        assert_eq!(first.session_id(), second.session_id());
        assert_ne!(first.base_session_id(), second.base_session_id());
        let mut other_persona = first.clone();
        other_persona.persona = Some(persona("persona-B"));
        assert_ne!(first.session_id(), other_persona.session_id());
        assert_eq!(first.base_session_id(), other_persona.base_session_id());
        other_persona.persona.as_mut().unwrap().revision = 2;
        assert_eq!(
            other_persona.session_id(),
            {
                let mut same = first;
                same.persona = Some(persona("persona-B"));
                same.session_id()
            },
            "editing the same character should retain its group history"
        );
    }

    #[test]
    fn shared_history_isolates_bot_platform_group_namespace_and_channel() {
        let original = context(vec![], true);
        let original_key = original.session_id();
        let mut other_bot = original.clone();
        other_bot.native.as_mut().unwrap().bot.instance_id =
            nuono_domain::NativeId::new("instance-B").unwrap();
        let mut other_account = original.clone();
        other_account.native.as_mut().unwrap().bot.account_id =
            identity(IdentityKind::Bot, "bot-B", Some("tenant-A"));
        let mut other_platform = original.clone();
        other_platform.native.as_mut().unwrap().bot.account_id = IdentityRef::new(
            Platform::Telegram,
            IdentityKind::Bot,
            IdentityScheme::Numeric,
            "10001",
            None::<&str>,
        )
        .unwrap();
        let mut other_group = original.clone();
        other_group.native.as_mut().unwrap().conversation = Some(
            ConversationRef::group(identity(IdentityKind::Group, "group-B", Some("tenant-A")))
                .unwrap(),
        );
        let mut other_namespace = original.clone();
        other_namespace.native.as_mut().unwrap().conversation = Some(
            ConversationRef::group(identity(IdentityKind::Group, "group-A", Some("tenant-B")))
                .unwrap(),
        );
        for context in [
            other_bot,
            other_account,
            other_platform,
            other_group,
            other_namespace,
        ] {
            assert_ne!(original_key, context.session_id());
        }
        let mut channel = original;
        channel.native.as_mut().unwrap().conversation = Some(
            ConversationRef::channel(
                identity(IdentityKind::Channel, "channel-A", Some("tenant-A")),
                Some(identity(IdentityKind::Group, "group-A", Some("tenant-A"))),
            )
            .unwrap(),
        );
        let mut sibling = channel.clone();
        sibling.native.as_mut().unwrap().conversation = Some(
            ConversationRef::channel(
                identity(IdentityKind::Channel, "channel-B", Some("tenant-A")),
                Some(identity(IdentityKind::Group, "group-A", Some("tenant-A"))),
            )
            .unwrap(),
        );
        let mut second_member = channel.clone();
        second_member.native.as_mut().unwrap().actor =
            identity(IdentityKind::User, "member-B", Some("tenant-A"));
        assert_eq!(channel.session_id(), second_member.session_id());
        assert_ne!(channel.session_id(), sibling.session_id());
        assert_ne!(channel.session_id(), original_key);
        assert_ne!(channel.base_session_id(), second_member.base_session_id());
    }

    #[test]
    fn legacy_group_history_uses_bot_target_and_platform_but_not_user() {
        let event = json!({
            "post_type": "message", "platform": "onebot", "message_type": "group",
            "user_id": 1001, "group_id": 2002, "message": "hello"
        });
        let first = AgentContext::from_runtime_event(42, &event).unwrap();
        let mut second = first.clone();
        second.user_id = 1002;
        assert_eq!(first.session_id(), second.session_id());
        assert_eq!(first.base_session_id(), "onebot:2002:1001");
        assert_ne!(first.base_session_id(), second.base_session_id());
        for changed in [
            AgentContext {
                bot_id: 43,
                ..first.clone()
            },
            AgentContext {
                platform: "telegram".into(),
                ..first.clone()
            },
            AgentContext {
                group_id: Some(2003),
                ..first.clone()
            },
            AgentContext {
                channel_id: Some("sub-channel".into()),
                ..first.clone()
            },
        ] {
            assert_ne!(first.session_id(), changed.session_id());
        }
    }

    #[test]
    fn private_sessions_and_plain_content_keep_existing_behavior() {
        let native = context(vec![MessageSegment::text("hello")], false);
        assert_eq!(native.session_id(), native.base_session_id());
        assert_eq!(native.user_message("hello").content, "hello");
        let legacy = AgentContext::from_runtime_event(
            42,
            &json!({
                "post_type": "message", "message_type": "private", "user_id": 1001,
                "message": "hello"
            }),
        )
        .unwrap();
        assert_eq!(legacy.session_id(), "onebot:private:1001");
        assert_eq!(legacy.user_message("hello").content, "hello");
    }

    #[test]
    fn extracts_typed_images_and_reply_without_promoting_unknown_raw_data() {
        let context = context(
            vec![
                MessageSegment::text("look "),
                MessageSegment::At {
                    user_id: "bot-A".into(),
                },
                image(FileSource::Url("https://example.com/image.png".into())),
                MessageSegment::Reply {
                    message_id: "previous-message".into(),
                },
                MessageSegment::Unknown {
                    kind: "image".into(),
                    data: json!({"url": "https://example.com/hidden.png", "reply": "fake"}),
                },
                MessageSegment::text("here"),
            ],
            true,
        );
        assert_eq!(context.content, "look here");
        assert_eq!(context.images, vec!["https://example.com/image.png"]);
        assert_eq!(
            context.reply_to_message_id.as_deref(),
            Some("previous-message")
        );
        assert!(context.native.as_ref().unwrap().mentioned);
        let message = context.user_message("look here");
        assert_eq!(message.images, context.images);
        assert!(message.content.contains("member-A"));
        assert!(message.content.contains("tenant-A"));
        assert!(message.content.contains("message-A"));
        assert!(!message.content.contains("hidden.png"));
    }

    #[test]
    fn images_alone_have_a_triggerable_placeholder() {
        let incoming = context(
            vec![image(FileSource::Url(
                "https://example.com/image.png".into(),
            ))],
            false,
        );
        assert_eq!(incoming.content, "[图片]");
        assert_eq!(incoming.user_message(&incoming.content).images.len(), 1);
        let unavailable = context(
            vec![image(FileSource::Path("/private/image.png".into()))],
            false,
        );
        assert!(unavailable.content.contains("无法读取"));
        assert!(unavailable.images.is_empty());
        assert!(!unavailable.content.contains("/private/image.png"));
    }

    #[test]
    fn a_quote_without_text_preserves_its_reference_as_input() {
        let incoming = context(
            vec![
                MessageSegment::At {
                    user_id: "bot-A".into(),
                },
                MessageSegment::Reply {
                    message_id: "previous-message".into(),
                },
            ],
            true,
        );
        assert_eq!(incoming.content, "[引用消息]");
        assert_eq!(
            incoming.reply_to_message_id.as_deref(),
            Some("previous-message")
        );
        assert!(incoming.native.as_ref().unwrap().mentioned);
    }

    #[test]
    fn limits_deduplicates_images_and_explains_unsupported_sources() {
        let mut segments = vec![MessageSegment::text("question")];
        for index in 0..5 {
            segments.push(image(FileSource::Url(format!(
                "https://example.com/{index}.png"
            ))));
        }
        segments.extend([
            image(FileSource::Url("https://example.com/0.png".into())),
            image(FileSource::Base64("data:image/png;base64,secret".into())),
            image(FileSource::Id("platform-image-id".into())),
            image(FileSource::Raw("https://example.com/not-typed.png".into())),
            image(FileSource::Url("https://".into())),
            image(FileSource::Url(
                "https://user:pass@example.com/image.png".into(),
            )),
            image(FileSource::Url(format!(
                "https://example.com/{}",
                "x".repeat(MAX_IMAGE_URL_BYTES)
            ))),
            image(FileSource::Url(
                "https://example.com/white space.png".into(),
            )),
        ]);
        let context = context(segments, false);
        assert_eq!(context.images.len(), MAX_IMAGES);
        assert!(context.content.contains("超过 4 张"));
        assert!(context.content.contains("无法读取"));
        assert!(!context.content.contains("secret"));
        assert!(!context.content.contains("platform-image-id"));
        assert!(
            context
                .images
                .iter()
                .all(|image| image.len() <= MAX_IMAGE_URL_BYTES)
        );
    }

    #[test]
    fn history_round_trip_preserves_speaker_media_and_reference_metadata() {
        let context = context(
            vec![
                MessageSegment::text("hello"),
                image(FileSource::Url("https://example.com/image.png".into())),
                MessageSegment::Reply {
                    message_id: "previous".into(),
                },
            ],
            true,
        );
        let raw = context.history_content("clean hello");
        let stored = HistoryMessage::decode(USER_CONTEXT_ROLE, &raw).unwrap();
        assert_eq!(stored.speaker, context.native.as_ref().unwrap().actor);
        assert_eq!(stored.content, "clean hello");
        assert_eq!(stored.message_id.as_deref(), Some("message-A"));
        assert_eq!(stored.reply_to_message_id.as_deref(), Some("previous"));
        assert_eq!(stored.chat_message().images, context.images);
        assert!(stored.reply_excerpt().contains("clean hello"));
        assert!(stored.reply_excerpt().contains("member-A"));
    }

    #[test]
    fn legacy_json_text_is_never_interpreted_as_a_history_envelope() {
        let context = context(vec![], false);
        let raw = context.history_content("user wrote JSON");
        assert!(HistoryMessage::decode("user", &raw).is_none());
        assert!(HistoryMessage::decode("assistant", &raw).is_none());
        assert!(HistoryMessage::decode(USER_CONTEXT_ROLE, "old plain text").is_none());
        let mut unsupported: serde_json::Value = serde_json::from_str(&raw).unwrap();
        unsupported["version"] = json!(2);
        assert!(HistoryMessage::decode(USER_CONTEXT_ROLE, &unsupported.to_string()).is_none());
        unsupported["version"] = json!(1);
        unsupported["speaker"]["kind"] = json!("bot");
        assert!(HistoryMessage::decode(USER_CONTEXT_ROLE, &unsupported.to_string()).is_none());
    }

    #[test]
    fn old_serialized_contexts_default_new_media_fields() {
        let context = context(vec![MessageSegment::text("hello")], false);
        let mut raw = serde_json::to_value(&context).unwrap();
        raw.as_object_mut().unwrap().remove("images");
        raw.as_object_mut().unwrap().remove("reply_to_message_id");
        let restored: AgentContext = serde_json::from_value(raw).unwrap();
        assert!(restored.images.is_empty());
        assert!(restored.reply_to_message_id.is_none());
        assert_eq!(restored.session_id(), context.session_id());
    }
}
