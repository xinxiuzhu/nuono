//! 触发器和结构化命令的并发注册、排序与调度。
//!
//! 所有类型的触发器在处理消息前合并，并按 `priority` 升序执行；同优先级保持当前注册
//! 顺序但不应作为稳定 API 依赖。命中本身不会停止传播，只有触发器的 `block` 标志会在
//! 处理器返回后终止后续调度。处理器错误被记录而不向上传播，返回值只表示是否发生阻断。

use crate::api::Bot;
use crate::core::command::Command;
use crate::core::trigger::{CommandOrigin, MatchData, Trigger, TriggerType};
use crate::onebot::Message;
use dashmap::DashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// 触发器注册表，管理所有注册的触发器
pub struct TriggerRegistry {
    /// 按 TriggerType 分类存储触发器
    triggers: DashMap<TriggerType, Vec<Arc<Trigger>>>,
    /// 结构化指令列表（用于帮助文档和 UI 展示）
    commands: DashMap<String, Arc<Command>>,
    /// 插件当前正在注册命令/触发器。
    registration_scope: StdRwLock<Option<String>>,
    /// 触发器总数统计
    trigger_count: RwLock<usize>,
}

impl std::fmt::Debug for TriggerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TriggerRegistry")
            .field("trigger_buckets", &self.triggers.len())
            .field("command_count", &self.commands.len())
            .finish_non_exhaustive()
    }
}

impl TriggerRegistry {
    /// 创建新的触发器注册表
    pub fn new() -> Self {
        Self {
            triggers: DashMap::new(),
            commands: DashMap::new(),
            registration_scope: StdRwLock::new(None),
            trigger_count: RwLock::new(0),
        }
    }

    /// 设置后续注册的默认插件归属。
    ///
    /// 插件宿主应把它作为短生命周期的注册上下文使用并及时清空；显式写在命令或触发器
    /// 上的 `plugin_name` 不会被覆盖。
    pub fn set_registration_scope(&self, plugin_name: Option<&str>) {
        if let Ok(mut scope) = self.registration_scope.write() {
            *scope = plugin_name.map(ToOwned::to_owned);
        }
    }

    fn current_registration_scope(&self) -> Option<String> {
        self.registration_scope
            .read()
            .ok()
            .and_then(|scope| scope.clone())
    }

    /// 注册结构化指令
    pub fn register_command(&self, mut command: Command) {
        if command.plugin_name.is_none() {
            command.plugin_name = self.current_registration_scope();
        }
        let key = Self::command_key(
            command.plugin_name.as_deref(),
            command.sv_name.as_deref(),
            &command.name,
        );
        for mut trigger in command.to_triggers() {
            trigger.origin_command = Some(CommandOrigin {
                plugin_name: command.plugin_name.clone(),
                sv_name: command.sv_name.clone(),
                command_name: command.name.clone(),
            });
            self.register(trigger);
        }
        self.commands.insert(key, Arc::new(command));
    }

    pub fn get_all_commands(&self) -> Vec<Arc<Command>> {
        self.commands.iter().map(|c| c.value().clone()).collect()
    }

    pub fn toggle_command(&self, name: &str, enabled: bool) -> bool {
        if let Some(mut cmd) = self.commands.get_mut(name) {
            let cmd_val = Arc::get_mut(&mut *cmd);
            if let Some(c) = cmd_val {
                c.enabled = enabled;
                return true;
            }
        }

        let mut changed = false;
        for mut entry in self.commands.iter_mut() {
            let cmd = entry.value_mut();
            if cmd.name != name {
                continue;
            }
            if let Some(command) = Arc::get_mut(cmd) {
                command.enabled = enabled;
                changed = true;
            }
        }
        changed
    }

    pub fn toggle_command_scoped(
        &self,
        plugin_name: Option<&str>,
        sv_name: Option<&str>,
        name: &str,
        enabled: bool,
    ) -> bool {
        let key = Self::command_key(plugin_name, sv_name, name);
        if let Some(mut cmd) = self.commands.get_mut(&key) {
            let cmd_val = Arc::get_mut(&mut *cmd);
            if let Some(c) = cmd_val {
                c.enabled = enabled;
                return true;
            }
        }
        false
    }

    pub fn apply_disabled_commands(&self, plugin_name: &str, disabled_commands: &[String]) {
        let disabled: std::collections::HashSet<&str> =
            disabled_commands.iter().map(String::as_str).collect();

        for mut entry in self.commands.iter_mut() {
            let cmd = entry.value_mut();
            if cmd.plugin_name.as_deref() != Some(plugin_name) {
                continue;
            }
            if let Some(command) = Arc::get_mut(cmd) {
                let scoped_key =
                    Self::command_config_key(command.sv_name.as_deref(), &command.name);
                command.enabled = !disabled.contains(command.name.as_str())
                    && !disabled.contains(scoped_key.as_str());
            }
        }
    }

    pub fn command_config_key(sv_name: Option<&str>, name: &str) -> String {
        match sv_name {
            Some(sv) if !sv.is_empty() => format!("{sv}::{name}"),
            _ => name.to_string(),
        }
    }

    pub fn command_key(plugin_name: Option<&str>, sv_name: Option<&str>, name: &str) -> String {
        format!(
            "{}::{}::{}",
            plugin_name.unwrap_or(""),
            sv_name.unwrap_or(""),
            name
        )
    }

    /// 注册一个触发器
    pub fn register(&self, mut trigger: Trigger) {
        if trigger.plugin_name.is_none() {
            trigger.plugin_name = self.current_registration_scope();
        }
        let trigger_type = trigger.trigger_type;
        let pattern = trigger.pattern.clone();

        if let Some(mut triggers) = self.triggers.get_mut(&trigger_type) {
            triggers.push(Arc::new(trigger));
            // 数字越小优先级越高
            triggers.sort_by_key(|t| t.priority);
        } else {
            self.triggers.insert(trigger_type, vec![Arc::new(trigger)]);
        }

        if let Ok(mut count) = self.trigger_count.try_write() {
            *count += 1;
        }

        debug!("注册触发器: {:?} 模式: {}", trigger_type, pattern);
    }

    /// 批量注册触发器
    pub fn register_all(&self, triggers: Vec<Trigger>) {
        for trigger in triggers {
            self.register(trigger);
        }
    }

    /// 取消注册触发器（按模式）
    pub fn unregister(&self, trigger_type: TriggerType, pattern: &str) {
        if let Some(mut triggers) = self.triggers.get_mut(&trigger_type) {
            let original_len = triggers.len();
            triggers.retain(|t| t.pattern != pattern);
            let removed = original_len - triggers.len();

            if removed > 0 {
                if let Ok(mut count) = self.trigger_count.try_write() {
                    *count = count.saturating_sub(removed);
                }
                info!(
                    "取消注册 {} 个触发器: {:?} 模式: {}",
                    removed, trigger_type, pattern
                );
            }
        }
    }

    /// 按插件名称取消注册该插件的所有触发器与结构化命令。
    pub fn unregister_by_plugin(&self, plugin_name: &str) -> usize {
        let mut total_removed = 0;
        self.triggers.retain(|_ty, triggers| {
            let original = triggers.len();
            triggers.retain(|t| t.plugin_name.as_deref() != Some(plugin_name));
            total_removed += original - triggers.len();
            !triggers.is_empty()
        });
        // 同时删除该插件的结构化命令元数据（commands DashMap）。先前只清 triggers 会残留
        // 命令，导致 disable/reload/failed-activation 后命令仍对帮助/UI 可见。
        let command_keys = self
            .commands
            .iter()
            .filter(|entry| entry.value().plugin_name.as_deref() == Some(plugin_name))
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        let removed_commands = command_keys.len();
        for key in &command_keys {
            self.commands.remove(key);
        }

        if total_removed > 0 {
            if let Ok(mut count) = self.trigger_count.try_write() {
                *count = count.saturating_sub(total_removed);
            }
            info!(
                plugin = %plugin_name,
                removed = total_removed,
                removed_commands,
                "已按插件取消注册触发器与命令"
            );
        }
        total_removed
    }

    /// 按全局优先级调度一次消息。
    ///
    /// 被禁用命令及其别名会在匹配前跳过。`Ok(true)` 表示某个已执行触发器设置了
    /// `block`；处理器失败只写日志，仍按该触发器的 `block` 决定是否继续。
    pub async fn handle(&self, event: &Message, bot: &Bot) -> anyhow::Result<bool> {
        let mut blocked = false;
        let mut handled = false;

        let mut all_triggers: Vec<Arc<Trigger>> = Vec::new();
        for entry in self.triggers.iter() {
            all_triggers.extend(entry.value().iter().cloned());
        }
        all_triggers.sort_by_key(|t| t.priority);

        for trigger in all_triggers {
            if self.is_trigger_command_disabled(&trigger) {
                continue;
            }

            if trigger.trigger_type == TriggerType::Message {
                handled = true;

                if let Err(e) = trigger
                    .handler
                    .handle(bot, event, MatchData::default())
                    .await
                {
                    error!("触发器执行失败: {} - {}", trigger.pattern, e);
                }

                if trigger.block {
                    blocked = true;
                    break;
                }
            } else {
                if let Some(match_data) = trigger.matches(event) {
                    handled = true;
                    debug!(
                        "触发器匹配: {:?} 模式: {}",
                        trigger.trigger_type, trigger.pattern
                    );

                    if let Err(e) = trigger.handler.handle(bot, event, match_data).await {
                        error!("触发器执行失败: {:?} - {}", trigger.pattern, e);
                    }

                    if trigger.block {
                        blocked = true;
                        break;
                    }
                }
            }
        }

        if !handled {
            let raw_text = match event {
                Message::GroupMessage(gm) => &gm.raw_message,
                Message::PrivateMessage(pm) => &pm.raw_message,
            };
            debug!("⚠️ 没有触发器匹配此消息: {}", raw_text);
        }

        Ok(blocked)
    }

    /// 获取指定类型的所有触发器
    pub fn get_triggers(&self, trigger_type: TriggerType) -> Vec<Arc<Trigger>> {
        self.triggers
            .get(&trigger_type)
            .map(|triggers| triggers.clone())
            .unwrap_or_default()
    }

    pub fn get_triggers_by_type(&self, trigger_type: &str) -> Vec<Arc<Trigger>> {
        if trigger_type == "command" {
            let mut commands = self.get_triggers(TriggerType::Command);
            commands.extend(self.get_triggers(TriggerType::EventCommand));
            commands.sort_by_key(|trigger| trigger.priority);
            return commands;
        }
        let type_enum = match trigger_type {
            "regex" => TriggerType::Regex,
            "command" => TriggerType::Command,
            "prefix" => TriggerType::Prefix,
            "keyword" => TriggerType::Keyword,
            "fullmatch" => TriggerType::FullMatch,
            "suffix" => TriggerType::Suffix,
            "message" => TriggerType::Message,
            "file" => TriggerType::File,
            _ => return vec![],
        };
        self.triggers
            .get(&type_enum)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// 获取所有触发器
    pub fn get_all_triggers(&self) -> Vec<Arc<Trigger>> {
        let mut all = Vec::new();
        for triggers in self.triggers.iter() {
            all.extend(triggers.value().clone());
        }
        all
    }

    pub fn is_trigger_command_disabled(&self, trigger: &Trigger) -> bool {
        let key = Self::command_key(
            trigger.plugin_name.as_deref(),
            trigger.sv_name.as_deref(),
            &trigger.pattern,
        );
        if let Some(cmd) = self.commands.get(&key) {
            return !cmd.enabled;
        }

        self.commands.iter().any(|entry| {
            let cmd = entry.value();
            let same_plugin = cmd.plugin_name == trigger.plugin_name;
            let same_sv = cmd.sv_name == trigger.sv_name;
            same_plugin
                && same_sv
                && cmd.aliases.iter().any(|alias| alias == &trigger.pattern)
                && !cmd.enabled
        })
    }

    /// 获取触发器总数
    pub fn count(&self) -> usize {
        if let Ok(count) = self.trigger_count.try_read() {
            *count
        } else {
            0
        }
    }

    /// 按类型统计触发器数量
    pub fn count_by_type(&self, trigger_type: TriggerType) -> usize {
        self.triggers
            .get(&trigger_type)
            .map(|triggers| triggers.len())
            .unwrap_or(0)
    }

    /// 清空所有触发器与命令
    pub fn clear(&self) {
        self.triggers.clear();
        self.commands.clear();
        if let Ok(mut count) = self.trigger_count.try_write() {
            *count = 0;
        }
        warn!("已清空所有触发器与命令");
    }

    /// 打印触发器统计信息
    pub fn print_stats(&self) {
        let types = vec![
            (TriggerType::Prefix, "Prefix（前缀）"),
            (TriggerType::Suffix, "Suffix（后缀）"),
            (TriggerType::Keyword, "Keyword（关键词）"),
            (TriggerType::FullMatch, "FullMatch（完全匹配）"),
            (TriggerType::Command, "Command（命令）"),
            (TriggerType::EventCommand, "Command（事件命令）"),
            (TriggerType::Regex, "Regex（正则）"),
            (TriggerType::Message, "Message（任意消息）"),
            (TriggerType::File, "File（文件）"),
        ];

        let details = types
            .into_iter()
            .filter_map(|(t_type, name)| {
                let count = self.count_by_type(t_type);
                (count > 0).then(|| format!("{name}: {count}"))
            })
            .collect::<Vec<_>>()
            .join(", ");

        debug!(total = self.count(), details = %details, "触发器统计");
    }
}

impl Default for TriggerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 触发器注册器构建器
#[derive(Debug)]
pub struct TriggerRegistryBuilder {
    registry: TriggerRegistry,
}

impl TriggerRegistryBuilder {
    /// 创建新的构建器
    pub fn new() -> Self {
        Self {
            registry: TriggerRegistry::new(),
        }
    }

    /// 添加触发器
    pub fn add_trigger(self, trigger: Trigger) -> Self {
        self.registry.register(trigger);
        self
    }

    /// 批量添加触发器
    pub fn add_triggers(self, triggers: Vec<Trigger>) -> Self {
        self.registry.register_all(triggers);
        self
    }

    /// 构建注册表
    pub fn build(self) -> TriggerRegistry {
        self.registry
    }
}

impl Default for TriggerRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Bot;
    use crate::core::trigger::TriggerHandler;
    use crate::onebot::Message;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct TestHandler;

    #[async_trait]
    impl TriggerHandler for TestHandler {
        async fn handle(
            &self,
            _bot: &Bot,
            _event: &Message,
            _match_data: MatchData,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_registry_creation() {
        let registry = TriggerRegistry::new();
        assert_eq!(registry.count(), 0);
    }

    #[tokio::test]
    async fn test_register_trigger() {
        let registry = TriggerRegistry::new();
        let trigger = Trigger::new(
            TriggerType::Prefix,
            "test",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );

        registry.register(trigger);
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.count_by_type(TriggerType::Prefix), 1);
    }

    #[tokio::test]
    async fn test_unregister_trigger() {
        let registry = TriggerRegistry::new();
        let trigger = Trigger::new(
            TriggerType::Prefix,
            "test",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );

        registry.register(trigger);
        assert_eq!(registry.count(), 1);

        registry.unregister(TriggerType::Prefix, "test");
        assert_eq!(registry.count(), 0);
    }

    #[tokio::test]
    async fn test_builder() {
        let registry = TriggerRegistryBuilder::new()
            .add_trigger(Trigger::new(
                TriggerType::Prefix,
                "test1",
                "",
                false,
                false,
                Arc::new(TestHandler),
            ))
            .add_trigger(Trigger::new(
                TriggerType::Command,
                "test2",
                "",
                false,
                false,
                Arc::new(TestHandler),
            ))
            .build();

        assert_eq!(registry.count(), 2);
    }

    #[tokio::test]
    async fn unregister_by_plugin_clears_commands_too() {
        let registry = TriggerRegistry::new();
        registry.set_registration_scope(Some("MyPlugin"));
        registry.register_command(Command::from_message("cmd", Arc::new(TestHandler)));
        registry.set_registration_scope(None);
        assert_eq!(registry.get_all_commands().len(), 1);

        registry.unregister_by_plugin("MyPlugin");
        assert!(
            registry.get_all_commands().is_empty(),
            "unregister_by_plugin 应同时清除该插件的命令元数据"
        );
    }

    #[tokio::test]
    async fn clear_clears_commands_and_triggers() {
        let registry = TriggerRegistry::new();
        registry.set_registration_scope(Some("MyPlugin"));
        registry.register_command(Command::from_message("cmd", Arc::new(TestHandler)));
        registry.set_registration_scope(None);
        registry.register(Trigger::new(
            TriggerType::Keyword,
            "k",
            "/",
            false,
            false,
            Arc::new(TestHandler),
        ));
        assert!(!registry.get_all_commands().is_empty());
        assert!(registry.count() > 0);

        registry.clear();
        assert!(
            registry.get_all_commands().is_empty(),
            "clear 应同时清空 commands"
        );
        assert_eq!(registry.count(), 0);
    }

    #[tokio::test]
    async fn test_clear() {
        let registry = TriggerRegistry::new();

        for i in 0..5 {
            registry.register(Trigger::new(
                TriggerType::Prefix,
                format!("test{}", i),
                "",
                false,
                false,
                Arc::new(TestHandler),
            ));
        }

        assert_eq!(registry.count(), 5);
        registry.clear();
        assert_eq!(registry.count(), 0);
    }

    struct TestNativeHandler;

    #[async_trait]
    impl crate::core::command::CommandHandler for TestNativeHandler {
        async fn handle(
            &self,
            _bot: &Bot,
            _event: &crate::plugin::Event<'_>,
            _match_data: MatchData,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn native_cmd(pattern: &str) -> crate::core::command::Command {
        crate::core::command::Command::new(pattern, Arc::new(TestNativeHandler))
    }

    #[test]
    fn command_origins_cover_aliases_and_scope_without_marking_raw_rules() {
        let registry = TriggerRegistry::new();
        registry.set_registration_scope(Some("dice"));
        registry.register_command(
            Command::from_message("roll", Arc::new(TestHandler))
                .with_alias("r")
                .with_sv("legacy"),
        );
        registry.register_command(native_cmd("roll").with_alias("nr").with_sv("native"));
        registry.register(
            Trigger::new(
                TriggerType::Command,
                "roll",
                "",
                false,
                false,
                Arc::new(TestHandler),
            )
            .with_sv_name("legacy"),
        );

        let triggers = registry.get_all_triggers();
        assert_eq!(triggers.len(), 5);
        let raw = triggers
            .iter()
            .filter(|trigger| trigger.origin_command.is_none())
            .collect::<Vec<_>>();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].pattern, "roll");
        for trigger in triggers
            .iter()
            .filter(|trigger| trigger.origin_command.is_some())
        {
            let origin = trigger.origin_command.as_ref().unwrap();
            assert_eq!(origin.plugin_name.as_deref(), Some("dice"));
            assert_eq!(origin.sv_name, trigger.sv_name);
            assert_eq!(origin.command_name, "roll");
        }
        assert_eq!(
            triggers
                .iter()
                .filter(|trigger| trigger
                    .origin_command
                    .as_ref()
                    .is_some_and(|origin| origin.sv_name.as_deref() == Some("legacy")))
                .count(),
            2
        );
        assert_eq!(
            triggers
                .iter()
                .filter(|trigger| trigger
                    .origin_command
                    .as_ref()
                    .is_some_and(|origin| origin.sv_name.as_deref() == Some("native")))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn command_registers_triggers_and_metadata() {
        let registry = TriggerRegistry::new();
        registry.register_command(
            native_cmd("roll")
                .with_alias("r")
                .with_plugin("dice")
                .with_description("掷骰子"),
        );
        // 主命 pattern + alias 各投影一条 native 触发器。
        assert_eq!(registry.count_by_type(TriggerType::EventCommand), 2);
        let native_triggers = registry.get_triggers(TriggerType::EventCommand);
        assert!(native_triggers.iter().all(|t| t.event_handler.is_some()));
        // Dashboard 命令元数据必须对 native 命令可见（不能因为调度入口不同而不可见）。
        let commands = registry.get_all_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "roll");
        assert_eq!(commands[0].trigger_type, TriggerType::EventCommand);
        assert_eq!(commands[0].aliases, vec!["r"]);
        assert!(commands[0].plugin_name.as_deref() == Some("dice"));
    }

    #[tokio::test]
    async fn native_and_legacy_triggers_coexist_and_sort_by_priority() {
        let registry = TriggerRegistry::new();
        let mut native = native_cmd("n");
        native.priority = 5;
        registry.register_command(native);

        let mut legacy = Trigger::new(
            TriggerType::Command,
            "l",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );
        legacy.priority = 1;
        registry.register(legacy);

        // 注册表同时保留两种调度入口的触发器，优先顺序确定（数字小者优先）。
        assert_eq!(registry.count_by_type(TriggerType::EventCommand), 1);
        assert_eq!(registry.count_by_type(TriggerType::Command), 1);
        let all = registry.get_all_triggers();
        assert!(all.iter().any(|t| t.is_event_command()));
        assert!(all.iter().any(|t| t.trigger_type == TriggerType::Command));
    }

    #[tokio::test]
    async fn unregister_by_plugin_clears_command_metadata() {
        let registry = TriggerRegistry::new();
        registry.set_registration_scope(Some("dice"));
        registry.register_command(Command::from_message("legacy", Arc::new(TestHandler)));
        registry.register_command(native_cmd("roll"));
        registry.set_registration_scope(None);
        assert_eq!(registry.get_all_commands().len(), 2);
        assert!(registry.count() > 0);

        registry.unregister_by_plugin("dice");
        assert!(registry.get_all_commands().is_empty());
        assert_eq!(registry.count(), 0);
    }

    #[tokio::test]
    async fn clear_clears_command_metadata() {
        let registry = TriggerRegistry::new();
        registry.set_registration_scope(Some("dice"));
        registry.register_command(native_cmd("roll"));
        registry.set_registration_scope(None);
        assert!(!registry.get_all_commands().is_empty());
        assert!(registry.count() > 0);

        registry.clear();
        assert!(registry.get_all_commands().is_empty());
        assert_eq!(registry.count(), 0);
    }
}
