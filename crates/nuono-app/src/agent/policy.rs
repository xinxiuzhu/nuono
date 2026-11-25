//! Agent 上下文与工具暴露策略。
//!
//! 策略采用默认允许：allow list 为空表示不限制，一旦非空则只允许命中项；deny list 总是
//! 优先。匹配支持精确值、全局 `*` 和仅位于末尾的前缀通配符。插件名单只作用于
//! `plugin.<PluginName>.*` 工具，不能替代工具风险等级、人工审批或工具内部授权。
//! 按人维度的 AI 准入由账号实体上的 `blocked_accounts` 统一控制，不再维护孤立的原始 ID 名单。

use serde::{Deserialize, Serialize};

use crate::settings::AIConfig;

use super::AgentContext;

/// 由 AI 运行配置生成的有效 Agent 策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AgentPolicy {
    pub allowed_platforms: Vec<String>,
    pub denied_platforms: Vec<String>,
    pub blocked_accounts: Vec<i64>,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub allowed_plugins: Vec<String>,
    pub denied_plugins: Vec<String>,
}

impl AgentPolicy {
    pub fn from_ai_config(cfg: &AIConfig) -> Self {
        Self {
            allowed_platforms: normalize_list(&cfg.agent_allowed_platforms),
            denied_platforms: normalize_list(&cfg.agent_denied_platforms),
            blocked_accounts: cfg.blocked_accounts.clone(),
            allowed_tools: normalize_list(&cfg.agent_allowed_tools),
            denied_tools: normalize_list(&cfg.agent_denied_tools),
            allowed_plugins: normalize_list(&cfg.agent_allowed_plugins),
            denied_plugins: normalize_list(&cfg.agent_denied_plugins),
        }
    }

    pub fn should_handle_context(&self, ctx: &AgentContext) -> bool {
        self.context_rejection(ctx).is_none()
    }

    pub fn context_rejection(&self, ctx: &AgentContext) -> Option<String> {
        if ctx.platform == super::ACCOUNT_CHAT_PLATFORM {
            if ctx.account_chat.is_none() {
                return Some("account chat requires an authenticated account context".into());
            }
            if let Some(account_id) = ctx.account_chat_owner()
                && self.blocked_accounts.contains(&account_id)
            {
                return Some("该账号已被管理员暂停使用 AI".into());
            }
        }
        if denied_or_not_allowed(
            &ctx.platform,
            &self.allowed_platforms,
            &self.denied_platforms,
        ) {
            return Some(format!(
                "platform `{}` rejected by agent policy",
                ctx.platform
            ));
        }

        None
    }

    pub fn allows_tool(&self, tool_name: &str) -> bool {
        self.tool_rejection(tool_name).is_none()
    }

    /// 返回工具不可暴露/不可执行的原因。
    ///
    /// 调用方应在生成模型可见 schema 和真正执行前都应用同一策略，防止模型伪造一个未
    /// 暴露的工具名绕过 allow/deny 检查。
    pub fn tool_rejection(&self, tool_name: &str) -> Option<String> {
        if pattern_list_matches(&self.denied_tools, tool_name) {
            return Some(format!("tool `{tool_name}` rejected by agent policy"));
        }
        if !self.allowed_tools.is_empty() && !pattern_list_matches(&self.allowed_tools, tool_name) {
            return Some(format!(
                "tool `{tool_name}` is not in agent tool allow list"
            ));
        }

        if let Some(plugin_name) = plugin_name_from_tool(tool_name) {
            if pattern_list_matches(&self.denied_plugins, plugin_name) {
                return Some(format!("plugin `{plugin_name}` rejected by agent policy"));
            }
            if !self.allowed_plugins.is_empty()
                && !pattern_list_matches(&self.allowed_plugins, plugin_name)
            {
                return Some(format!(
                    "plugin `{plugin_name}` is not in agent plugin allow list"
                ));
            }
        }

        None
    }
}

fn normalize_list(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn denied_or_not_allowed(value: &str, allowed: &[String], denied: &[String]) -> bool {
    pattern_list_matches(denied, value)
        || (!allowed.is_empty() && !pattern_list_matches(allowed, value))
}

fn pattern_list_matches(patterns: &[String], value: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern_matches(pattern, value))
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

fn plugin_name_from_tool(tool_name: &str) -> Option<&str> {
    tool_name
        .strip_prefix("plugin.")
        .and_then(|rest| rest.split('.').next())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> AgentContext {
        AgentContext {
            account_chat: None,
            images: Vec::new(),
            reply_to_message_id: None,
            persona: None,
            native: None,
            bot_id: 42,
            platform: "onebot".to_string(),
            message_type: "group".to_string(),
            user_id: 1001,
            group_id: Some(2002),
            channel_id: None,
            message_id: None,
            raw_message: "hello".to_string(),
            content: "hello".to_string(),
        }
    }

    #[test]
    fn deny_platform_wins_over_allow() {
        let policy = AgentPolicy {
            allowed_platforms: vec!["onebot".to_string()],
            denied_platforms: vec!["onebot".to_string()],
            ..Default::default()
        };

        assert!(!policy.should_handle_context(&ctx()));
    }

    #[test]
    fn blocked_account_rejects_account_chat() {
        let policy = AgentPolicy {
            blocked_accounts: vec![7],
            ..Default::default()
        };

        assert!(!policy.should_handle_context(&AgentContext::for_assistant(7, "hi".into())));
        assert!(policy.should_handle_context(&AgentContext::for_assistant(8, "hi".into())));
    }

    #[test]
    fn tool_patterns_support_prefix_wildcard() {
        let policy = AgentPolicy {
            allowed_tools: vec!["plugin.PingPlugin.*".to_string()],
            denied_tools: vec!["plugin.PingPlugin.ping.delete".to_string()],
            ..Default::default()
        };

        assert!(policy.allows_tool("plugin.PingPlugin.ping.info"));
        assert!(!policy.allows_tool("plugin.PingPlugin.ping.delete"));
        assert!(!policy.allows_tool("conversation.describe"));
    }

    #[test]
    fn plugin_policy_applies_to_plugin_tools_only() {
        let policy = AgentPolicy {
            denied_plugins: vec!["PingPlugin".to_string()],
            ..Default::default()
        };

        assert!(!policy.allows_tool("plugin.PingPlugin.ping.info"));
        assert!(policy.allows_tool("conversation.describe"));
    }
}
