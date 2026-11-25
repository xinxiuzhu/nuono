//! AI 回复的输出投递策略配置（完整发送、原生流式、分段发送）及流式输出进度快照。
use super::AgentContext;
use nuono_domain::{BotIdentity, ConversationRef};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// 单次流式模型请求可发布的累计文本上限。
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// 宿主如何呈现仅回复的 Agent 答案。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// 等完整答案就绪后一次性发送。
    #[default]
    Complete,
    /// 优先使用原生流式；不支持的会话退回完整投递。
    Native,
    /// 将连续文本分段作为多条消息发送。
    Segmented,
}

/// 投递时序与所选模型及其凭据无关。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputPolicy {
    /// 请求的呈现方式；默认完整投递。
    pub mode: OutputMode,
    /// 原生更新或文本分段之间的间隔（毫秒）。
    pub interval_ms: u64,
    /// 单个文本分段的 Unicode 标量值上限。
    pub segment_chars: usize,
}
impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            mode: OutputMode::Complete,
            interval_ms: 1000,
            segment_chars: 400,
        }
    }
}
impl OutputPolicy {
    /// 校验支持的时序与分段大小边界。
    ///
    /// # Errors
    /// 越界时返回配置错误。
    pub fn validate(&self) -> Result<()> {
        if !(300..=10_000).contains(&self.interval_ms) || !(80..=2000).contains(&self.segment_chars)
        {
            return Err(NuonoError::ConfigValidation(
                "AI 输出间隔须为 300–10000 毫秒，每段须为 80–2000 字符".into(),
            ));
        }
        Ok(())
    }
}

/// 精确的 bot 覆盖项，可再限定到单个原生会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputOverride {
    /// 完整 bot 身份，含实例、账号与租户。
    pub bot: BotIdentity,
    /// 完整原生会话；缺省则作用于整个 bot。
    #[serde(default)]
    pub conversation: Option<ConversationRef>,
    /// 对所选作用域的完整替换策略。
    pub policy: OutputPolicy,
}

/// 会话覆盖优先于 bot 覆盖与默认策略。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputSettings {
    /// 没有精确原生覆盖匹配时使用的策略。
    pub default: OutputPolicy,
    /// 最多 256 条 bot 或会话策略。
    pub overrides: Vec<OutputOverride>,
}
impl OutputSettings {
    /// 在不改变身份作用域的前提下选择会话、bot 或默认策略。
    pub fn resolve(&self, ctx: &AgentContext) -> OutputPolicy {
        let Some(native) = &ctx.native else {
            return self.default.clone();
        };
        self.overrides
            .iter()
            .find(|item| {
                item.bot == native.bot
                    && item.conversation.is_some()
                    && item.conversation == native.conversation
            })
            .or_else(|| {
                self.overrides
                    .iter()
                    .find(|item| item.bot == native.bot && item.conversation.is_none())
            })
            .map(|item| &item.policy)
            .unwrap_or(&self.default)
            .clone()
    }
    /// 校验策略、作用域唯一性与汇总设置预算。
    ///
    /// # Errors
    /// 拒绝无效参数、重复作用域与超限设置。
    pub fn validate(&self) -> Result<()> {
        self.default.validate()?;
        if self.overrides.len() > 256 || serde_json::to_vec(self)?.len() > 128 * 1024 {
            return Err(NuonoError::ConfigValidation(
                "AI 输出覆盖设置超出数量或大小限制".into(),
            ));
        }
        let mut seen = HashSet::new();
        for item in &self.overrides {
            item.policy.validate()?;
            if !seen.insert((&item.bot, &item.conversation)) {
                return Err(NuonoError::ConfigValidation(
                    "AI 输出包含重复会话覆盖".into(),
                ));
            }
            if let Some(conversation) = &item.conversation {
                let matches = match conversation {
                    ConversationRef::Direct {
                        participant,
                        conversation,
                    } => {
                        participant.platform() == item.bot.platform()
                            && conversation
                                .as_ref()
                                .is_none_or(|id| id.platform() == item.bot.platform())
                    }
                    ConversationRef::Group { group } => group.platform() == item.bot.platform(),
                    ConversationRef::Channel { channel, parent } => {
                        channel.platform() == item.bot.platform()
                            && parent
                                .as_ref()
                                .is_none_or(|id| id.platform() == item.bot.platform())
                    }
                    _ => conversation.platform() == item.bot.platform(),
                };
                if !matches {
                    return Err(NuonoError::ConfigValidation(
                        "AI 输出覆盖的机器人与会话平台不一致".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// 流式输出进度快照（通过 `watch` 通道仅保留最新累计文本，不逐 token 排队；
/// `complete` 仅代表模型生成完毕，不代表平台已投递成功）。
#[derive(Debug, Clone, Default)]
pub struct OutputProgress {
    /// 截至目前已生成的累计文本（不超过 `MAX_OUTPUT_BYTES`），便于接收方直接合并中间更新。
    pub content: String,
    /// 提供方已成功结束；投递仍由宿主负责。
    pub complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_config_defaults_to_complete_and_invalid_modes_are_rejected() {
        let settings: OutputSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.default.mode, OutputMode::Complete);
        settings.validate().unwrap();
        for value in [
            serde_json::json!({"mode":"typo"}),
            serde_json::json!({"interval_ms":299}),
            serde_json::json!({"segment_chars":0}),
        ] {
            assert!(
                serde_json::from_value::<OutputPolicy>(value)
                    .and_then(|policy| policy.validate().map_err(serde::de::Error::custom))
                    .is_err()
            );
        }
    }

    #[test]
    fn exact_conversation_overrides_win_without_crossing_instances_or_namespaces() {
        use nuono_domain::{IdentityKind, IdentityRef, IdentityScheme, Platform};
        let identity = |kind, value: &str| {
            IdentityRef::new(
                Platform::QqOfficial,
                kind,
                IdentityScheme::OpenId,
                value,
                Some("app"),
            )
            .unwrap()
        };
        let bot = BotIdentity::new("instance", identity(IdentityKind::Bot, "app"), None).unwrap();
        let conversation =
            ConversationRef::group(identity(IdentityKind::Group, "native-group")).unwrap();
        let mut ctx = AgentContext::from_runtime_event(
            1,
            &serde_json::json!({"post_type":"message","user_id":2,"message":"hi"}),
        )
        .unwrap();
        ctx.native = Some(super::super::AgentNativeContext {
            bot: bot.clone(),
            actor: identity(IdentityKind::User, "member"),
            conversation: Some(conversation.clone()),
            mentioned: true,
        });
        let mut settings = OutputSettings {
            default: Default::default(),
            overrides: vec![
                OutputOverride {
                    bot: bot.clone(),
                    conversation: None,
                    policy: OutputPolicy {
                        mode: OutputMode::Native,
                        ..Default::default()
                    },
                },
                OutputOverride {
                    bot,
                    conversation: Some(conversation),
                    policy: OutputPolicy {
                        mode: OutputMode::Segmented,
                        ..Default::default()
                    },
                },
            ],
        };
        settings.validate().unwrap();
        assert_eq!(settings.resolve(&ctx).mode, OutputMode::Segmented);
        ctx.native.as_mut().unwrap().conversation = None;
        assert_eq!(settings.resolve(&ctx).mode, OutputMode::Native);
        ctx.native.as_mut().unwrap().bot.instance_id =
            nuono_domain::NativeId::new("another").unwrap();
        assert_eq!(settings.resolve(&ctx).mode, OutputMode::Complete);
        ctx.native.as_mut().unwrap().bot = BotIdentity::new(
            "instance",
            IdentityRef::new(
                Platform::QqOfficial,
                IdentityKind::Bot,
                IdentityScheme::OpenId,
                "app",
                Some("other-app"),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(settings.resolve(&ctx).mode, OutputMode::Complete);
        settings.overrides.push(settings.overrides[1].clone());
        assert!(settings.validate().is_err());
    }
}
