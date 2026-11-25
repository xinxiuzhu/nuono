use crate::api::Bot;
use crate::core::trigger::{MatchData, Trigger, TriggerHandler, TriggerType};
use crate::plugin::Event;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use nuono_app::permission::{ActorPermission, PermissionLevel};

/// 指令处理器，接收完整事件；平台字符串 ID 与路由保持原样。
#[async_trait]
pub trait CommandHandler: Send + Sync {
    async fn handle(&self, bot: &Bot, event: &Event<'_>, data: MatchData) -> anyhow::Result<()>;
}

/// 结构化指令定义
#[derive(Clone, Serialize, Deserialize)]
pub struct Command {
    /// 稳定的指令匹配模式与管理端唯一标识。
    pub name: String,
    /// 可选的管理端可读显示名称（仅用于界面展示，绝不参与指令匹配或作为配置存储键）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub aliases: Vec<String>,
    pub description: String,
    pub usage: String,
    pub permission: PermissionLevel,
    pub priority: i32,
    #[serde(default = "default_command_block")]
    pub block: bool,
    #[serde(default)]
    pub to_me: bool,
    #[serde(skip)]
    pub handler: Option<Arc<dyn CommandHandler>>,
    /// 接收 OneBot Message 的处理器；宏根据函数参数选择，无需声明插件类型。
    #[serde(skip)]
    pub message_handler: Option<Arc<dyn TriggerHandler>>,
    pub plugin_name: Option<String>,
    pub sv_name: Option<String>,
    #[serde(default)]
    pub trigger_type: TriggerType,
    pub enabled: bool,
    /// 是否剥离前缀（对齐 gsuid_core）
    pub strip_prefix: bool,
}

impl Command {
    pub fn new(pattern: impl Into<String>, handler: Arc<dyn CommandHandler>) -> Self {
        Self {
            name: pattern.into(),
            trigger_type: TriggerType::EventCommand,
            handler: Some(handler),
            ..Self::default()
        }
    }

    /// 创建接收 OneBot Message 的指令。
    pub fn from_message(name: impl Into<String>, handler: Arc<dyn TriggerHandler>) -> Self {
        Self {
            name: name.into(),
            display_name: None,
            aliases: vec![],
            description: String::new(),
            usage: String::new(),
            trigger_type: TriggerType::Command,
            permission: PermissionLevel::Everyone,
            priority: 0,
            block: true,
            to_me: false,
            handler: None,
            message_handler: Some(handler),
            plugin_name: None,
            sv_name: None,
            enabled: true,
            strip_prefix: true,
        }
    }

    /// 设置管理界面中展示的可读名称，不影响实际的指令匹配模式。
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.display_name = (!name.trim().is_empty()).then_some(name);
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// 设置文本匹配方式（与处理函数接收 `&Event` 还是 `&Message` 无关）。
    /// `Prefix` 允许参数紧跟在指令词后而无需空格分隔；`Regex` 则会向处理函数提供位置捕获组与具名捕获组。
    pub fn with_match_type(mut self, match_type: TriggerType) -> Self {
        self.trigger_type = match_type;
        self
    }

    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn with_aliases(mut self, aliases: &[impl AsRef<str>]) -> Self {
        self.aliases
            .extend(aliases.iter().map(|alias| alias.as_ref().to_string()));
        self
    }

    pub fn with_usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = usage.into();
        self
    }

    pub fn with_permission(mut self, level: PermissionLevel) -> Self {
        self.permission = level;
        self
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_block(mut self, block: bool) -> Self {
        self.block = block;
        self
    }

    pub fn with_to_me(mut self, to_me: bool) -> Self {
        self.to_me = to_me;
        self
    }

    pub fn with_plugin(mut self, name: impl Into<String>) -> Self {
        self.plugin_name = Some(name.into());
        self
    }

    pub fn with_sv(mut self, name: impl Into<String>) -> Self {
        self.sv_name = Some(name.into());
        self
    }

    pub fn no_strip_prefix(mut self) -> Self {
        self.strip_prefix = false;
        self
    }

    pub fn to_triggers(&self) -> Vec<Trigger> {
        let mut patterns = vec![self.name.clone()];
        patterns.extend(self.aliases.iter().cloned());

        patterns
            .into_iter()
            .map(|p| {
                let mut t = if let Some(handler) = &self.handler {
                    Trigger::new_event(
                        self.trigger_type,
                        p,
                        self.block,
                        self.to_me,
                        handler.clone(),
                    )
                } else {
                    Trigger::new(
                        self.trigger_type,
                        p,
                        "",
                        self.block,
                        self.to_me,
                        self.message_handler
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| Arc::new(MockHandler)),
                    )
                };
                t.priority = self.priority;
                t.plugin_name = self.plugin_name.clone();
                t.sv_name = self.sv_name.clone();
                t.required_permission = Some(self.permission);
                t
            })
            .collect()
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("permission", &self.permission)
            .field("enabled", &self.enabled)
            .finish()
    }
}

struct MockHandler;
#[::async_trait::async_trait]
impl crate::core::trigger::TriggerHandler for MockHandler {
    async fn handle(
        &self,
        _: &crate::api::Bot,
        _: &crate::onebot::Message,
        _: crate::core::trigger::MatchData,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Default for Command {
    fn default() -> Self {
        Self {
            name: String::new(),
            display_name: None,
            aliases: vec![],
            description: String::new(),
            usage: String::new(),
            permission: PermissionLevel::Everyone,
            priority: 0,
            block: true,
            to_me: false,
            handler: None,
            message_handler: None,
            plugin_name: None,
            sv_name: None,
            trigger_type: TriggerType::Command,
            enabled: true,
            strip_prefix: true,
        }
    }
}

fn default_command_block() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestNativeHandler;

    #[async_trait]
    impl CommandHandler for TestNativeHandler {
        async fn handle(
            &self,
            _bot: &Bot,
            _event: &Event<'_>,
            _match_data: MatchData,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn display_metadata_does_not_change_patterns_aliases_or_legacy_serialization() {
        let command = Command::new("^面板(.+)$", Arc::new(TestNativeHandler))
            .with_sv("角色")
            .with_alias("^card(.+)$")
            .with_display_name("角色面板");
        assert_eq!(command.display_name.as_deref(), Some("角色面板"));
        let triggers = command.to_triggers();
        assert_eq!(triggers[0].pattern, "^面板(.+)$");
        assert_eq!(triggers[1].pattern, "^card(.+)$");
        let mut legacy = serde_json::to_value(&command).unwrap();
        legacy.as_object_mut().unwrap().remove("display_name");
        let restored: Command = serde_json::from_value(legacy).unwrap();
        assert!(restored.display_name.is_none());
        assert_eq!(restored.sv_name.as_deref(), Some("角色"));
    }

    fn cmd() -> Command {
        Command::new("roll", Arc::new(TestNativeHandler))
            .with_alias("r")
            .with_description("掷骰子")
            .with_usage("roll [count]")
            .with_permission(PermissionLevel::GroupAdmin)
            .with_priority(3)
            .with_block(false)
            .with_to_me(true)
            .with_plugin("dice")
            .with_sv("骰子服务")
    }

    #[test]
    fn to_triggers_preserves_pattern_aliases_and_permissions() {
        let triggers = cmd().to_triggers();
        assert_eq!(triggers.len(), 2);
        assert!(triggers.iter().all(|t| {
            t.trigger_type == TriggerType::EventCommand
                && t.event_handler.is_some()
                && t.required_permission == Some(PermissionLevel::GroupAdmin)
                && t.plugin_name.as_deref() == Some("dice")
                && t.sv_name.as_deref() == Some("骰子服务")
                && t.priority == 3
                && !t.block
                && t.to_me
        }));
        // 主命pattern必须排在最前，alias紧随其后。
        assert_eq!(triggers[0].pattern, "roll");
        assert_eq!(triggers[1].pattern, "r");
    }

    #[test]
    fn serialization_preserves_ui_and_toggle_fields() {
        let meta: Command = serde_json::from_value(serde_json::to_value(cmd()).unwrap()).unwrap();
        assert_eq!(meta.name, "roll");
        assert_eq!(meta.aliases, vec!["r"]);
        assert_eq!(meta.description, "掷骰子");
        assert_eq!(meta.usage, "roll [count]");
        assert_eq!(meta.permission, PermissionLevel::GroupAdmin);
        assert_eq!(meta.priority, 3);
        assert!(!meta.block);
        assert!(meta.to_me);
        assert_eq!(meta.plugin_name.as_deref(), Some("dice"));
        assert_eq!(meta.sv_name.as_deref(), Some("骰子服务"));
        assert_eq!(meta.trigger_type, TriggerType::EventCommand);
        assert!(meta.enabled);
        // 接收 Event 的指令不会经由 Message 处理器 调度。
        assert!(meta.handler.is_none());
        assert!(meta.message_handler.is_none());
    }

    fn text_message(text: &str) -> crate::onebot::Message {
        crate::onebot::parse_message(
            &serde_json::json!({
                "time": 0, "self_id": 1, "post_type": "message", "message_type": "private",
                "sub_type": "friend", "message_id": 1, "user_id": 1, "message": text,
                "raw_message": text, "font": 0, "sender": {"user_id": 1, "nickname": "test"}
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn event_prefix_accepts_attached_arguments_while_default_keeps_command_boundary() {
        let command = Command::new("设置阈值", Arc::new(TestNativeHandler));
        assert!(
            command.to_triggers()[0]
                .matches(&text_message("设置阈值235"))
                .is_none()
        );
        let triggers = command
            .with_match_type(TriggerType::Prefix)
            .with_alias("阈值")
            .to_triggers();
        for (trigger, input) in triggers.iter().zip(["设置阈值235", "阈值 240"]) {
            assert!(trigger.is_event_command());
            assert_eq!(
                trigger.matches(&text_message(input)).unwrap().args,
                if input.ends_with("235") { "235" } else { "240" }
            );
        }
    }

    #[test]
    fn event_regex_compiles_captures_alias_patterns_and_rejects_invalid_regex() {
        let triggers = Command::new("^(?P<name>.+)面板$", Arc::new(TestNativeHandler))
            .with_alias("^(?P<name>.+)面包$")
            .with_match_type(TriggerType::Regex)
            .to_triggers();
        for (trigger, input) in triggers.iter().zip(["今汐面板", "今夕面包"]) {
            assert!(trigger.is_event_command());
            let matched = trigger.matches(&text_message(input)).unwrap();
            assert!(matched.regex_named_groups["name"].starts_with('今'));
            assert_eq!(matched.regex_groups.len(), 1);
        }
        let invalid = Command::new("[", Arc::new(TestNativeHandler))
            .with_match_type(TriggerType::Regex)
            .to_triggers();
        assert!(invalid[0].is_event_command());
        assert!(invalid[0].matches(&text_message("[")).is_none());
    }

    #[test]
    fn event_full_match_requires_the_entire_text() {
        let trigger = Command::new("面板", Arc::new(TestNativeHandler))
            .with_match_type(TriggerType::FullMatch)
            .to_triggers()
            .remove(0);
        assert!(trigger.is_event_command());
        assert!(trigger.matches(&text_message("面板")).is_some());
        assert!(trigger.matches(&text_message("面板 今汐")).is_none());
    }
}
