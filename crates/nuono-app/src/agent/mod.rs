//! Agent 核心模块：定义自主等级、执行上下文、策略与工具接口，并通过模型提供方与存储层接口实现运行时编排。
use serde::{Deserialize, Serialize};
pub mod children;
pub mod companion_memory;
pub mod contracts;
mod dag;
pub mod input;
mod lifecycle;
pub mod memory;
pub mod output;
pub mod policy;
mod resources;
mod scheduling;
pub use resources::AgentResourceLimits;
#[cfg(test)]
mod resource_tests;
mod semantic_memory;
pub mod tool;
pub use policy::AgentPolicy;
/// Agent 自主等级，决定哪些风险等级可以暴露给模型。
///
/// `ReplyOnly` 禁止工具；`LowRiskTools` 开放只读和低风险工具；`ApprovedTools` 额外允许
/// 模型提出需审批工具，但执行会停在待审批；`AdminApproved` 才允许包括管理员工具在内的
/// 全部已注册工具直接执行。AgentPolicy 仍可进一步收紧任何等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum AutonomyLevel {
    #[default]
    ReplyOnly,
    LowRiskTools,
    ApprovedTools,
    AdminApproved,
}

/// Agent 输入上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    /// 由宿主服务显式发起的后台账号对话上下文（外部聊天平台入站事件绝不会设置此字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_chat: Option<AccountChatContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<crate::persona::PersonaSnapshot>,
    #[serde(default)]
    pub native: Option<AgentNativeContext>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub raw_message: String,
    pub content: String,
    /// 经过校验且带数量限制（最多 4 张）的 HTTP(S) 图片链接，用于传递给多模态模型。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

/// 账号级对话的专用会话键前缀，与外部聊天平台身份完全隔离。
pub const ACCOUNT_CHAT_PREFIX: &str = "account-chat:v1:";
/// 账号级对话在策略匹配中使用的固定平台标识。
pub const ACCOUNT_CHAT_PLATFORM: &str = "account_chat";
const ASSISTANT_NAMESPACE: &str = "core/personal-assistant";

/// 账号级对话的会话标识及由宿主服务指定的系统提示词上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountChatContext {
    key: String,
    system_prompt: String,
    #[serde(skip)]
    personal_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_scope: Option<String>,
    #[serde(default)]
    assistant: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
}

/// 在指定服务命名空间内为已认证账号生成稳定的会话作用域标识。
pub fn account_chat_key(namespace: &str, account_id: i64) -> String {
    use sha2::Digest;
    let namespace = hex::encode(sha2::Sha256::digest(namespace.as_bytes()));
    format!("{ACCOUNT_CHAT_PREFIX}{namespace}:{account_id}")
}

/// 从账号对话作用域标识中解析出宿主账号 ID（不会将外部聊天平台的用户 ID 误识别为宿主账号）。
pub fn account_chat_owner(key: &str) -> Option<i64> {
    let rest = key.strip_prefix(ACCOUNT_CHAT_PREFIX)?;
    let mut parts = rest.split(':');
    let namespace = parts.next()?;
    if namespace.len() != 64 || !namespace.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    parts.next()?.parse().ok()
}

pub fn assistant_chat_key(account_id: i64) -> String {
    account_chat_key(ASSISTANT_NAMESPACE, account_id)
}

/// 单个伴侣角色卡的持久化记忆作用域标识（角色卡 UUID 的归属权由存储层校验）。
pub fn companion_scope_key(account_id: i64, card_id: &str) -> String {
    format!("{}:card:{card_id}", assistant_chat_key(account_id))
}

/// 伴侣模型上下文的历史记录键（触发遗忘时通过递增 `epoch` 轮换模型上下文，而无需擦除用户可见的历史归档）。
pub fn companion_history_key(scope_key: &str, epoch: i64) -> String {
    format!("{scope_key}:context:{epoch}")
}

impl AgentContext {
    pub fn with_personal_profile(
        mut self,
        profile: &crate::personal_profile::PersonalProfile,
    ) -> Self {
        if let Some(chat) = &mut self.account_chat
            && chat.assistant
        {
            chat.personal_context = profile.ai_context();
        }
        self
    }

    pub fn for_companion_turn(account_id: i64, turn: &crate::assistant::AssistantTurn) -> Self {
        let mut ctx = Self::for_assistant_turn(account_id, turn.id.clone(), turn.text.clone());
        if let Some(chat) = &mut ctx.account_chat {
            chat.key = companion_history_key(&turn.scope_key, turn.context_epoch);
            chat.memory_scope = Some(turn.scope_key.clone());
            chat.system_prompt = turn.prompt_snapshot.clone();
        }
        ctx
    }
    /// 构造带轮次 ID（`turn_id`）的个人助手对话上下文，由宿主通过轮次回执统一管理历史记录与重试。
    pub fn for_assistant_turn(account_id: i64, turn_id: String, content: String) -> Self {
        let mut ctx = Self::for_assistant(account_id, content);
        if let Some(chat) = &mut ctx.account_chat {
            chat.turn_id = Some(turn_id);
        }
        ctx
    }

    pub(super) fn assistant_turn_id(&self) -> Option<&str> {
        self.account_chat
            .as_ref()
            .filter(|chat| chat.assistant)?
            .turn_id
            .as_deref()
    }

    /// 若当前上下文属于已认证的账号对话，返回对应的宿主账号 ID。
    pub fn account_chat_owner(&self) -> Option<i64> {
        self.account_chat
            .as_ref()
            .and_then(|chat| account_chat_owner(&chat.key))
    }

    pub fn for_assistant(account_id: i64, content: String) -> Self {
        let mut ctx = Self::for_account_chat(
            ASSISTANT_NAMESPACE,
            account_id,
            content,
            "你是 Nuono 的个人 AI 助手。自然、诚实地回答，并参考当前账号的对话上下文。".into(),
        );
        if let Some(chat) = &mut ctx.account_chat {
            chat.assistant = true;
        }
        ctx
    }

    pub(super) fn enabled_in(&self, cfg: &crate::settings::AIConfig) -> bool {
        if self
            .account_chat
            .as_ref()
            .is_some_and(|chat| chat.assistant)
        {
            cfg.assistant_active()
        } else {
            cfg.reply_active()
        }
    }

    /// 为已完成身份认证与权限校验的账号构造通用文本对话上下文。
    pub fn for_account_chat(
        namespace: &str,
        account_id: i64,
        content: String,
        system_prompt: String,
    ) -> Self {
        Self {
            account_chat: Some(AccountChatContext {
                key: account_chat_key(namespace, account_id),
                memory_scope: None,
                system_prompt,
                personal_context: None,
                assistant: false,
                turn_id: None,
            }),
            persona: None,
            native: None,
            bot_id: 0,
            platform: ACCOUNT_CHAT_PLATFORM.into(),
            message_type: "private".into(),
            user_id: account_id,
            group_id: None,
            channel_id: None,
            message_id: None,
            raw_message: String::new(),
            content,
            images: Vec::new(),
            reply_to_message_id: None,
        }
    }

    pub fn from_runtime_event(bot_id: i64, event: &serde_json::Value) -> Option<Self> {
        if event.get("post_type").and_then(|v| v.as_str()) != Some("message") {
            return None;
        }

        let raw_message = event
            .get("raw_message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let content = event
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(&raw_message)
            .to_string();

        let user_id = event.get("user_id")?.as_i64()?;
        if user_id == 0 {
            return None;
        }
        let group_id = match event.get("group_id") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(value.as_i64()?),
        };

        Some(Self {
            account_chat: None,
            persona: None,
            native: None,
            bot_id,
            platform: event
                .get("platform")
                .and_then(|v| v.as_str())
                .unwrap_or("onebot")
                .to_string(),
            message_type: event
                .get("message_type")
                .and_then(|v| v.as_str())
                .unwrap_or("private")
                .to_string(),
            user_id,
            group_id,
            channel_id: event
                .get("channel_id")
                .and_then(value_to_string)
                .or_else(|| event.get("chat_id").and_then(value_to_string)),
            message_id: event.get("message_id").and_then(value_to_string),
            raw_message,
            content,
            images: Vec::new(),
            reply_to_message_id: None,
        })
    }

    pub fn from_domain_event(event: &nuono_domain::DomainEvent) -> Option<Self> {
        use nuono_domain::ConversationRef;
        let message = event.message()?;
        let actor = event
            .actor
            .clone()
            .unwrap_or_else(|| message.sender.clone());
        let (message_type, group, channel) = match event.conversation.as_ref() {
            Some(ConversationRef::Group { group }) => {
                ("group", Some(group.value().to_string()), None)
            }
            Some(ConversationRef::Channel { channel, parent }) => (
                "channel",
                parent.as_ref().map(|p| p.value().to_string()),
                Some(channel.value().to_string()),
            ),
            _ => ("private", None, None),
        };
        let mentioned=message.segments.iter().any(|segment|matches!(segment,nuono_domain::MessageSegment::At {user_id} if user_id==event.bot.account_id.value().as_str()));
        let input = input::MessageInput::from_segments(&message.segments);
        let content = input.content;
        Some(Self {
            account_chat: None,
            persona: None,
            native: Some(AgentNativeContext {
                bot: event.bot.clone(),
                actor: actor.clone(),
                conversation: event.conversation.clone(),
                mentioned,
            }),
            bot_id: event.bot.account_id.value().as_str().parse().unwrap_or(0),
            platform: event.bot.platform().to_string(),
            message_type: message_type.into(),
            user_id: actor.value().as_str().parse().unwrap_or(0),
            group_id: group.as_deref().and_then(|g| g.parse().ok()),
            channel_id: channel,
            message_id: Some(message.message_id.to_string()),
            raw_message: content.clone(),
            content,
            images: input.images,
            reply_to_message_id: input.reply_to_message_id,
        })
    }
    pub fn user_identity(&self) -> String {
        self.native
            .as_ref()
            .map(|n| n.actor.value().to_string())
            .unwrap_or_else(|| self.user_id.to_string())
    }
    pub fn group_identity(&self) -> Option<String> {
        use nuono_domain::ConversationRef;
        match self.native.as_ref().and_then(|n| n.conversation.as_ref()) {
            Some(ConversationRef::Group { group }) => Some(group.value().to_string()),
            Some(ConversationRef::Channel { channel, parent }) => {
                Some(parent.as_ref().unwrap_or(channel).value().to_string())
            }
            _ => self.group_id.map(|g| g.to_string()),
        }
    }
    pub fn user_scope_key(&self) -> String {
        if let Some(chat) = &self.account_chat {
            return chat.memory_scope.as_ref().unwrap_or(&chat.key).clone();
        }
        match &self.native {
            Some(native) if native.actor.namespace().is_some() => identity_scope_key(&native.actor),
            _ => format!("{}:{}", self.platform, self.user_identity()),
        }
    }
    pub fn group_scope_key(&self) -> Option<String> {
        use nuono_domain::ConversationRef;
        let identity = match self.native.as_ref().and_then(|n| n.conversation.as_ref()) {
            Some(ConversationRef::Group { group }) => Some(group),
            Some(ConversationRef::Channel { channel, parent }) => {
                Some(parent.as_ref().unwrap_or(channel))
            }
            _ => None,
        };
        if let Some(identity) = identity.filter(|id| id.namespace().is_some()) {
            return Some(identity_scope_key(identity));
        }
        self.group_identity()
            .map(|id| format!("{}:{id}", self.platform))
    }
    /// 获取用于读取对话历史的会话 ID（群聊与频道由全体成员共享同一历史上下文，但并发运行锁仍按发言人隔离）。
    pub fn session_id(&self) -> String {
        if self.is_shared_conversation() {
            use sha2::Digest;
            let persona = self.persona.as_ref().map(|persona| &persona.id);
            let canonical = match &self.native {
                Some(native) => serde_json::to_vec(&(
                    "group_history_v1",
                    &native.bot,
                    &native.conversation,
                    persona,
                )),
                None => serde_json::to_vec(&(
                    "group_history_v1",
                    &self.platform,
                    self.bot_id,
                    &self.message_type,
                    self.group_id,
                    &self.channel_id,
                    persona,
                )),
            }
            .expect("conversation identity serializes");
            return format!("group:{}", hex::encode(sha2::Sha256::digest(canonical)));
        }
        if let Some(persona) = &self.persona {
            use sha2::Digest;
            let identity = serde_json::to_vec(&(
                self.base_session_id(),
                self.bot_id,
                &self.channel_id,
                &persona.id,
            ))
            .expect("persona session serializes");
            return format!("persona:{}", hex::encode(sha2::Sha256::digest(identity)));
        }
        self.base_session_id()
    }
    pub fn is_shared_conversation(&self) -> bool {
        use nuono_domain::ConversationRef;
        if let Some(native) = &self.native {
            return matches!(
                native.conversation,
                Some(ConversationRef::Group { .. } | ConversationRef::Channel { .. })
            );
        }
        matches!(self.message_type.as_str(), "group" | "channel")
            && (self.group_id.is_some() || self.channel_id.is_some())
    }

    /// 获取按单个发言人隔离的基础会话 ID（用于活跃运行控制，且在群聊切换角色卡时保持稳定）。
    pub fn base_session_id(&self) -> String {
        if let Some(chat) = &self.account_chat {
            return chat.key.clone();
        }
        if let Some(native) = &self.native {
            use sha2::Digest;
            let canonical = serde_json::to_vec(&(&native.bot, &native.conversation, &native.actor))
                .expect("domain identity serializes");
            return format!("v2:{}", hex::encode(sha2::Sha256::digest(canonical)));
        }
        self.legacy_session_id()
    }
    pub fn legacy_session_id(&self) -> String {
        match self.group_id {
            Some(group_id) => format!("{}:{}:{}", self.platform, group_id, self.user_id),
            None => format!("{}:private:{}", self.platform, self.user_id),
        }
    }
}

fn value_to_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|id| id.to_string()))
        .or_else(|| value.as_u64().map(|id| id.to_string()))
}

pub mod store;

pub mod ports;
pub mod runtime;

pub mod execution;
pub mod run;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNativeContext {
    pub bot: nuono_domain::BotIdentity,
    pub actor: nuono_domain::IdentityRef,
    pub conversation: Option<nuono_domain::ConversationRef>,
    pub mentioned: bool,
}

fn identity_scope_key(identity: &nuono_domain::IdentityRef) -> String {
    use sha2::Digest;
    format!(
        "v2:{}",
        hex::encode(sha2::Sha256::digest(
            serde_json::to_vec(identity).expect("identity serializes")
        ))
    )
}

#[cfg(test)]
mod personal_profile_tests {
    use super::*;
    #[test]
    fn personal_profile_only_enriches_personal_assistant_and_respects_opt_out() {
        let profile = crate::personal_profile::PersonalProfile {
            display_name: "Alice".into(),
            use_for_ai: true,
            ..Default::default()
        };
        let general = AgentContext::for_account_chat("other", 1, "hello".into(), "assigned".into())
            .with_personal_profile(&profile);
        assert!(general.account_chat.unwrap().personal_context.is_none());
        let personal =
            AgentContext::for_assistant(1, "hello".into()).with_personal_profile(&profile);
        let chat = personal.account_chat.as_ref().unwrap();
        assert!(chat.personal_context.as_ref().unwrap().contains("Alice"));
        // 个人资料仅在运行时作为临时背景注入，绝不会覆盖角色卡设定，也不会被序列化落盘。
        assert!(!chat.system_prompt.contains("Alice"));
        assert!(!serde_json::to_string(&personal).unwrap().contains("Alice"));
        let opt_out = personal.with_personal_profile(&crate::personal_profile::PersonalProfile {
            use_for_ai: false,
            ..profile
        });
        assert!(opt_out.account_chat.unwrap().personal_context.is_none());
    }
}
