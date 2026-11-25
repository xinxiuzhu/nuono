//! OneBot 兼容触发器的匹配语义。
//!
//! `Command` 直接匹配插件处理后的业务命令名，并要求其后是空白、常见标点或字符串结尾，
//! 避免 `ping` 误命中 `pingpong`。命令名、Prefix/FullMatch 的字面名称忽略 ASCII 大小写，参数原样保留。
//! 其他内容触发器仍将自身
//! `prefix` 作为匹配条件；`Message` 不检查文本，`File` 匹配文件名扩展名。匹配成功仅
//! 产生 [`MatchData`]，执行顺序与阻断语义由 [`crate::core::TriggerRegistry`] 决定。

use crate::api::Bot;
use crate::onebot::{Message, MessageSegment};
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{trace, warn};

/// 触发器类型
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[non_exhaustive]
pub enum TriggerType {
    /// 前缀匹配
    Prefix,
    /// 后缀匹配
    Suffix,
    /// 关键词包含
    Keyword,
    /// 完全匹配
    FullMatch,
    /// 命令匹配（前缀+命令）
    #[default]
    Command,
    /// 使用完整 Event 的命令处理器。
    EventCommand,
    /// 正则表达式匹配
    Regex,
    /// 任意消息（不检查内容）
    Message,
    /// 文件类型匹配
    File,
}

/// 匹配数据，提取自触发器匹配过程
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MatchData {
    /// 完整匹配的内容
    pub full_match: String,
    /// 正则分组捕获
    pub regex_groups: Vec<String>,
    /// 正则命名分组捕获
    pub regex_named_groups: HashMap<String, String>,
    /// 提取的参数（除去触发器的部分）
    pub args: String,
}

impl MatchData {
    /// 创建新的匹配数据
    pub fn new(full_match: String) -> Self {
        Self {
            full_match,
            regex_groups: Vec::new(),
            regex_named_groups: HashMap::new(),
            args: String::new(),
        }
    }

    /// 设置正则分组
    pub fn with_regex_groups(mut self, groups: Vec<String>) -> Self {
        self.regex_groups = groups;
        self
    }

    /// 设置命名分组
    pub fn with_named_groups(mut self, named: HashMap<String, String>) -> Self {
        self.regex_named_groups = named;
        self
    }

    /// 设置参数
    pub fn with_args(mut self, args: String) -> Self {
        self.args = args;
        self
    }

    /// 获取指定索引的分组
    pub fn group(&self, index: usize) -> Option<&str> {
        self.regex_groups.get(index).map(String::as_str)
    }

    /// 获取命名分组
    pub fn named(&self, name: &str) -> Option<&str> {
        self.regex_named_groups.get(name).map(String::as_str)
    }
}

impl Default for MatchData {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// 生成此触发器的结构化指令。主名称与注册范围共同标识所属功能。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOrigin {
    pub plugin_name: Option<String>,
    pub sv_name: Option<String>,
    pub command_name: String,
}

/// 触发器定义
#[derive(Clone)]
pub struct Trigger {
    /// 触发器类型
    pub trigger_type: TriggerType,
    /// 匹配模式（关键词、正则等）
    pub pattern: String,
    /// 命令前缀
    pub prefix: String,
    /// 处理器运行后是否停止本次注册表调度。
    ///
    /// 即使处理器返回错误也会阻断；该标志不撤销已执行处理器的副作用。
    pub block: bool,
    /// 是否只响应 @机器人的消息
    pub to_me: bool,
    /// 优先级（数字越小优先级越高）
    pub priority: i32,
    /// 触发器处理器
    pub handler: Arc<dyn TriggerHandler>,
    /// 结构化命令权限（仅命令触发器使用）
    pub required_permission: Option<crate::core::command::PermissionLevel>,
    /// 编译后的正则表达式（缓存）
    pub regex: Option<Regex>,
    /// 所属插件名称
    pub plugin_name: Option<String>,
    /// 所属 SV 名称
    pub sv_name: Option<String>,
    /// 仅结构化指令注册时设置；直接注册的规则没有指令来源。
    pub origin_command: Option<CommandOrigin>,
    /// Event 处理器；与文本匹配方式独立，可用于普通命令、前缀、正则等。
    ///
    /// 与 [`Self::handler`] 区分：接收 Event 的指令在调度期不经 OneBot 解析，直接用原始
    /// runtime `Value` 构造 [`crate::plugin::Event`] 调用此 handler。
    pub event_handler: Option<Arc<dyn crate::core::command::CommandHandler>>,
}

impl std::fmt::Debug for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trigger")
            .field("trigger_type", &self.trigger_type)
            .field("pattern", &self.pattern)
            .field("prefix", &self.prefix)
            .field("block", &self.block)
            .field("to_me", &self.to_me)
            .field("priority", &self.priority)
            .field("required_permission", &self.required_permission)
            .field("plugin_name", &self.plugin_name)
            .field("sv_name", &self.sv_name)
            .field("origin_command", &self.origin_command)
            .field("has_event_handler", &self.event_handler.is_some())
            .finish_non_exhaustive()
    }
}

impl Trigger {
    /// 创建新的触发器
    pub fn new(
        trigger_type: TriggerType,
        pattern: impl Into<String>,
        prefix: impl Into<String>,
        block: bool,
        to_me: bool,
        handler: Arc<dyn TriggerHandler>,
    ) -> Self {
        let pattern_str = pattern.into();
        let regex = if trigger_type == TriggerType::Regex {
            Regex::new(&pattern_str).ok()
        } else {
            None
        };

        Self {
            trigger_type,
            pattern: pattern_str,
            prefix: prefix.into(),
            block,
            to_me,
            priority: 0,
            handler,
            required_permission: None,
            regex,
            plugin_name: None,
            sv_name: None,
            origin_command: None,
            event_handler: None,
        }
    }

    /// 设置优先级
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// 设置所属插件名称
    pub fn with_plugin_name(mut self, name: impl Into<String>) -> Self {
        self.plugin_name = Some(name.into());
        self
    }

    /// 设置触发器内前缀（仅内容触发器使用）。
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// 设置触发器最低权限要求。
    pub fn with_permission(mut self, permission: crate::core::command::PermissionLevel) -> Self {
        self.required_permission = Some(permission);
        self
    }

    /// 创建指令触发器。
    ///
    /// 与 [`Self::new`] 的区别：命令的调度可直接从 [`crate::plugin::Event`] 读取平台原生
    /// 字符串 ID / route / raw，不经 OneBot 数值投影。`handler` 字段仍保留一个 no-op
    /// [`TriggerHandler`]（因为 [`Trigger`] 的字段布局要求它存在），真正的执行由
    /// `event_handler` 承担；调度器按 `is_event_command()` 选择处理器。
    pub fn new_event(
        trigger_type: TriggerType,
        pattern: impl Into<String>,
        block: bool,
        to_me: bool,
        event_handler: Arc<dyn crate::core::command::CommandHandler>,
    ) -> Self {
        let mut trigger = Self::new(
            trigger_type,
            pattern,
            "",
            block,
            to_me,
            Arc::new(EventCommandTriggerHandler),
        );
        trigger.event_handler = Some(event_handler);
        trigger
    }

    /// 是否直接调度 Event 处理器；正则、前缀等匹配方式不改变输入身份。
    pub fn is_event_command(&self) -> bool {
        self.event_handler.is_some()
    }

    /// 设置所属 SV 名称
    pub fn with_sv_name(mut self, name: impl Into<String>) -> Self {
        self.sv_name = Some(name.into());
        self
    }

    /// 检查事件是否匹配此触发器。
    ///
    /// 对 `Command`，`full_match` 是命令模式，`args` 是边界后的去空白文本；对 `Regex`，
    /// 正则只作用于去掉本触发器 `prefix` 后的文本，捕获组不包含 group 0。无效正则在
    /// 构造时缓存为 `None`，匹配时记录告警并视为不命中。
    pub fn matches(&self, event: &Message) -> Option<MatchData> {
        let raw_text = Self::extract_raw_text(event);
        let to_me = Self::is_at_me(event);
        let file_type = Self::get_file_type(event);
        self.matches_text(&raw_text, to_me, file_type.as_deref())
    }

    /// 跨平台事件匹配：直接基于规范 [`crate::plugin::Event`] 与已剥离插件前缀后的正文匹配，
    /// 不依赖 OneBot 数值解析，支持 QQ 官方、Telegram、Discord、飞书等全部平台。
    pub fn matches_event(
        &self,
        event: &crate::plugin::Event<'_>,
        candidate_text: &str,
    ) -> Option<MatchData> {
        let to_me = event.is_private()
            || event
                .domain_event()
                .is_some_and(|d| match &d.kind {
                    nuono_domain::DomainEventKind::Message(msg) => {
                        let bot_id = d.bot.account_id.value().as_str();
                        msg.segments.iter().any(|seg| {
                            matches!(seg, nuono_domain::MessageSegment::At { user_id } if user_id == bot_id)
                        })
                    }
                    _ => false,
                })
            || event
                .raw()
                .get("self_id")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })
                .is_some_and(|self_id| {
                    event
                        .message()
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|segments| {
                            segments.iter().any(|seg| {
                                seg.get("type").and_then(serde_json::Value::as_str) == Some("at")
                                    && seg
                                        .get("data")
                                        .and_then(|d| d.get("qq"))
                                        .and_then(|v| {
                                            v.as_str()
                                                .map(str::to_owned)
                                                .or_else(|| v.as_i64().map(|n| n.to_string()))
                                        })
                                        .as_deref()
                                        == Some(self_id.as_str())
                            })
                        })
                        || event.text().contains(&format!("[CQ:at,qq={self_id}]"))
                });
        let file_type = Self::extract_event_file_type(event);
        self.matches_text(candidate_text, to_me, file_type.as_deref())
    }

    /// 纯文本 / 跨平台匹配核心逻辑（OneBot `matches` 与跨平台 `matches_event` 共享同一实现）。
    pub fn matches_text(
        &self,
        raw_text: &str,
        is_at_me: bool,
        file_type: Option<&str>,
    ) -> Option<MatchData> {
        trace!(
            "🔍 触发器匹配检查 - 类型: {:?}, 模式: '{}', 前缀: '{}', 消息字节数: {}",
            self.trigger_type,
            self.pattern,
            self.prefix,
            raw_text.len()
        );
        if raw_text.is_empty()
            && !matches!(self.trigger_type, TriggerType::Message | TriggerType::File)
        {
            return None;
        }

        if self.to_me && !is_at_me {
            return None;
        }

        match self.trigger_type {
            TriggerType::Prefix => {
                let full_prefix = format!("{}{}", self.prefix, self.pattern);
                if raw_text
                    .get(..full_prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(&full_prefix))
                {
                    let args = raw_text[full_prefix.len()..].trim().to_string();
                    let match_data = MatchData::new(full_prefix).with_args(args.clone());
                    trace!(
                        "Prefix 触发器匹配: {} -> 参数字节数: {}",
                        self.pattern,
                        args.len()
                    );
                    Some(match_data)
                } else {
                    None
                }
            }
            TriggerType::Suffix => {
                let suffix = &self.pattern;
                if raw_text.ends_with(suffix) && raw_text.starts_with(&self.prefix) {
                    let match_data = MatchData::new(suffix.to_string());
                    trace!("Suffix 触发器匹配: {}", suffix);
                    Some(match_data)
                } else {
                    None
                }
            }
            TriggerType::Keyword => {
                if raw_text.contains(&self.pattern) && raw_text.starts_with(&self.prefix) {
                    let match_data = MatchData::new(self.pattern.clone());
                    trace!("Keyword 触发器匹配: {}", self.pattern);
                    Some(match_data)
                } else {
                    None
                }
            }
            TriggerType::FullMatch => {
                let full_match = format!("{}{}", self.prefix, self.pattern);
                trace!("🔍 FullMatch 匹配: {}", full_match);
                if raw_text.eq_ignore_ascii_case(&full_match) {
                    let match_data = MatchData::new(full_match.clone());
                    trace!("✅ FullMatch 触发器匹配: {}", full_match);
                    Some(match_data)
                } else {
                    trace!("❌ FullMatch 触发器未匹配: {}", full_match);
                    None
                }
            }
            TriggerType::Command => {
                // handler.rs 已经剥离了插件业务前缀，所以这里直接匹配 pattern
                let command = &self.pattern;
                trace!("🔍 Command 匹配: {}", command);
                if raw_text
                    .get(..command.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(command))
                    && Self::command_boundary_ok(raw_text.get(command.len()..))
                {
                    let args = raw_text[command.len()..].trim().to_string();
                    let match_data = MatchData::new(command.clone()).with_args(args.clone());
                    trace!(
                        "✅ Command 触发器匹配: {} -> 参数字节数: {}",
                        self.pattern,
                        args.len()
                    );
                    Some(match_data)
                } else {
                    trace!("❌ Command 触发器未匹配: {}", command);
                    None
                }
            }
            TriggerType::EventCommand => {
                let command = &self.pattern;
                if raw_text
                    .get(..command.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(command))
                    && Self::command_boundary_ok(raw_text.get(command.len()..))
                {
                    let args = raw_text[command.len()..].trim().to_string();
                    Some(MatchData::new(command.clone()).with_args(args))
                } else {
                    None
                }
            }
            TriggerType::Regex => {
                if let Some(ref regex) = self.regex {
                    if raw_text.starts_with(&self.prefix) {
                        let text = &raw_text[self.prefix.len()..];
                        if let Some(captures) = regex.captures(text) {
                            let full_match = captures
                                .get(0)
                                .map(|m| m.as_str().to_string())
                                .unwrap_or_default();
                            let groups: Vec<String> = captures
                                .iter()
                                .skip(1)
                                .map(|m| m.map(|c| c.as_str().to_string()).unwrap_or_default())
                                .collect();
                            let named_groups: HashMap<String, String> = regex
                                .capture_names()
                                .filter_map(|opt_name| {
                                    opt_name.and_then(|name| {
                                        captures
                                            .name(name)
                                            .map(|m| (name.to_string(), m.as_str().to_string()))
                                    })
                                })
                                .collect();
                            trace!(
                                "Regex 触发器匹配: {} ({} groups)",
                                self.pattern,
                                groups.len()
                            );
                            Some(
                                MatchData::new(full_match)
                                    .with_regex_groups(groups)
                                    .with_named_groups(named_groups),
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    warn!("正则表达式编译失败: {}", self.pattern);
                    None
                }
            }
            TriggerType::Message => {
                trace!("Message 触发器匹配（任意消息）");
                Some(MatchData::new(raw_text.to_string()))
            }
            TriggerType::File => {
                if let Some(file_type) = file_type {
                    if file_type.eq_ignore_ascii_case(&self.pattern) {
                        let match_data = MatchData::new(self.pattern.clone());
                        trace!("File 触发器匹配: {}", file_type);
                        Some(match_data)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    }

    fn extract_event_file_type(event: &crate::plugin::Event<'_>) -> Option<String> {
        let extract_ext = |name: &str| -> Option<String> {
            std::path::Path::new(name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .or_else(|| {
                    name.contains('.')
                        .then(|| name.rsplit('.').next().unwrap_or("").to_ascii_lowercase())
                })
        };
        if let Some(domain) = event.domain_event()
            && let nuono_domain::DomainEventKind::Message(msg) = &domain.kind
        {
            for seg in &msg.segments {
                if let nuono_domain::MessageSegment::File { name, .. } = seg
                    && let Some(ext) = extract_ext(name)
                {
                    return Some(ext);
                }
            }
        }
        if let Some(segments) = event.message().and_then(serde_json::Value::as_array) {
            for seg in segments {
                if seg.get("type").and_then(serde_json::Value::as_str) == Some("file")
                    && let Some(name) = seg
                        .get("data")
                        .and_then(|d| d.get("name"))
                        .and_then(serde_json::Value::as_str)
                    && let Some(ext) = extract_ext(name)
                {
                    return Some(ext);
                }
            }
        }
        None
    }

    fn command_boundary_ok(rest: Option<&str>) -> bool {
        let Some(rest) = rest else {
            return true;
        };
        let Some(ch) = rest.chars().next() else {
            return true;
        };

        ch.is_whitespace() || matches!(ch, '，' | '。' | '、' | ',' | '.' | '!' | '?' | ':' | ';')
    }

    /// 提取事件的原始消息文本
    fn extract_raw_text(event: &Message) -> String {
        match event {
            Message::GroupMessage(gm) => gm.raw_message.clone(),
            Message::PrivateMessage(pm) => pm.raw_message.clone(),
        }
    }

    /// 检查是否 @了机器人
    fn is_at_me(event: &Message) -> bool {
        match event {
            Message::GroupMessage(gm) => {
                for seg in &gm.message {
                    if let MessageSegment::At { data } = seg {
                        let qq_str = match &data.qq {
                            crate::onebot::AtType::All => "all".to_string(),
                            crate::onebot::AtType::Id(s) => s.clone(),
                        };
                        if qq_str == gm.self_id.to_string() {
                            return true;
                        }
                    }
                }
                false
            }
            Message::PrivateMessage(pm) => {
                for seg in &pm.message {
                    if let MessageSegment::At { data } = seg {
                        let qq_str = match &data.qq {
                            crate::onebot::AtType::All => "all".to_string(),
                            crate::onebot::AtType::Id(s) => s.clone(),
                        };
                        if qq_str == pm.self_id.to_string() {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }

    /// 获取文件类型（从消息段文件名提取扩展名并标准化为 ASCII 小写）
    fn get_file_type(event: &Message) -> Option<String> {
        let segments = match event {
            Message::GroupMessage(gm) => &gm.message,
            Message::PrivateMessage(pm) => &pm.message,
        };
        for seg in segments {
            if let MessageSegment::File { data } = seg {
                if let Some(ext) = std::path::Path::new(&data.name)
                    .extension()
                    .and_then(|e| e.to_str())
                {
                    return Some(ext.to_ascii_lowercase());
                }
                if data.name.find('.').is_some() {
                    return Some(
                        data.name
                            .rsplit('.')
                            .next()
                            .unwrap_or("")
                            .to_ascii_lowercase(),
                    );
                }
            }
        }
        None
    }
}

/// 触发器处理器 Trait
#[async_trait]
pub trait TriggerHandler: Send + Sync {
    /// 处理触发的事件
    ///
    /// # Arguments
    /// * `bot` - Bot 实例
    /// * `event` - OneBot 消息（Message 枚举）
    /// * `match_data` - 匹配数据
    async fn handle(&self, bot: &Bot, event: &Message, match_data: MatchData)
    -> anyhow::Result<()>;
}

/// 指令触发器的占位 [`TriggerHandler`]。
///
/// [`Trigger`] 字段布局要求 `handler` 恒非空。[`TriggerType::EventCommand`] 触发器的
/// 调度由 [`Trigger::event_handler`] 承担，这个 no-op 只满足结构约束，不会被调度器调用。
struct EventCommandTriggerHandler;

#[async_trait]
impl TriggerHandler for EventCommandTriggerHandler {
    async fn handle(
        &self,
        _bot: &Bot,
        _event: &Message,
        _match_data: MatchData,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 便捷宏：创建前缀触发器
#[macro_export]
macro_rules! prefix_trigger {
    ($pattern:expr, $handler:expr) => {
        Trigger::new(
            TriggerType::Prefix,
            $pattern,
            "",
            false,
            false,
            Arc::new($handler),
        )
    };
    ($pattern:expr, $handler:expr, block = $block:expr, to_me = $to_me:expr) => {
        Trigger::new(
            TriggerType::Prefix,
            $pattern,
            "",
            $block,
            $to_me,
            Arc::new($handler),
        )
    };
}

/// 便捷宏：创建命令触发器
#[macro_export]
macro_rules! command_trigger {
    ($pattern:expr, $handler:expr) => {
        Trigger::new(
            TriggerType::Command,
            $pattern,
            "",
            false,
            false,
            Arc::new($handler),
        )
    };
    ($pattern:expr, $handler:expr, block = $block:expr) => {
        Trigger::new(
            TriggerType::Command,
            $pattern,
            "",
            $block,
            false,
            Arc::new($handler),
        )
    };
}

/// 便捷宏：创建正则触发器
#[macro_export]
macro_rules! regex_trigger {
    ($pattern:expr, $handler:expr) => {
        Trigger::new(
            TriggerType::Regex,
            $pattern,
            "",
            false,
            false,
            Arc::new($handler),
        )
    };
    ($pattern:expr, $handler:expr, block = $block:expr) => {
        Trigger::new(
            TriggerType::Regex,
            $pattern,
            "",
            $block,
            false,
            Arc::new($handler),
        )
    };
}

pub use command_trigger;
pub use prefix_trigger;
pub use regex_trigger;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onebot::{PrivateMessage, PrivateMessageSender, TextData};
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

    #[test]
    fn test_trigger_creation() {
        let trigger = Trigger::new(
            TriggerType::Prefix,
            "test",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );

        assert_eq!(trigger.pattern, "test");
        assert_eq!(trigger.trigger_type, TriggerType::Prefix);
    }

    #[test]
    fn test_match_data() {
        let data = MatchData::new("full_match".to_string()).with_args("test args".to_string());

        assert_eq!(data.full_match, "full_match");
        assert_eq!(data.args, "test args");
    }

    #[test]
    fn test_match_data_with_groups() {
        let mut groups = HashMap::new();
        groups.insert("name".to_string(), "value".to_string());

        let data = MatchData::new("test".to_string())
            .with_regex_groups(vec!["group1".to_string(), "group2".to_string()])
            .with_named_groups(groups);

        assert_eq!(data.group(0), Some("group1"));
        assert_eq!(data.named("name"), Some("value"));
    }

    #[test]
    fn command_match_requires_boundary() {
        let trigger = Trigger::new(
            TriggerType::Command,
            "r",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );

        assert!(trigger.matches(&private_msg("r 100")).is_some());
        assert!(trigger.matches(&private_msg("r")).is_some());
        assert!(trigger.matches(&private_msg("nuono帮助")).is_none());
    }

    #[test]
    fn literal_command_names_ignore_ascii_case_without_lowercasing_arguments() {
        for kind in [
            TriggerType::Command,
            TriggerType::EventCommand,
            TriggerType::Prefix,
        ] {
            let trigger = Trigger::new(kind, "QQ音乐登录", "", false, false, Arc::new(TestHandler));
            for text in [
                "qq音乐登录 TokenAbC",
                "QQ音乐登录 TokenAbC",
                "qQ音乐登录 TokenAbC",
            ] {
                let data = trigger.matches(&private_msg(text)).unwrap();
                assert_eq!(data.args, "TokenAbC");
            }
            assert!(trigger.matches(&private_msg("你好")).is_none());
            if kind != TriggerType::Prefix {
                assert!(trigger.matches(&private_msg("qq音乐登录Extra")).is_none());
            }
        }
        let exact = Trigger::new(
            TriggerType::FullMatch,
            "QQ音乐登录",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );
        assert!(exact.matches(&private_msg("qq音乐登录")).is_some());
        assert!(exact.matches(&private_msg("qq音乐登录 extra")).is_none());
    }

    #[test]
    fn bare_file_trigger_normalizes_extension_without_changing_received_metadata() {
        for pattern in ["json", "JSON"] {
            let trigger = Trigger::new(
                TriggerType::File,
                pattern,
                "",
                false,
                false,
                Arc::new(TestHandler),
            );
            for name in [
                "records.json",
                "records.JSON",
                "records.JsOn",
                "records.backup.JSON",
            ] {
                let mut event = private_msg("");
                let Message::PrivateMessage(message) = &mut event else {
                    unreachable!()
                };
                message.message = vec![MessageSegment::File {
                    data: crate::onebot::FileData {
                        name: name.into(),
                        file: "file_id:opaque-native".into(),
                    },
                }];
                let result = trigger
                    .matches(&event)
                    .expect("bare file should match its extension");
                assert_eq!(result.full_match, pattern);
                assert_eq!(Trigger::get_file_type(&event).as_deref(), Some("json"));
                let Message::PrivateMessage(message) = event else {
                    unreachable!()
                };
                assert!(message.raw_message.is_empty());
                let MessageSegment::File { data } = &message.message[0] else {
                    unreachable!()
                };
                assert_eq!(data.name, name);
                assert_eq!(data.file, "file_id:opaque-native");
            }
        }
    }

    #[test]
    fn file_extension_matching_does_not_accept_text_or_trailing_fake_extensions() {
        let trigger = Trigger::new(
            TriggerType::File,
            "json",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );
        assert!(trigger.matches(&private_msg("records.JSON")).is_none());
        for name in [
            "records.JSON.exe",
            "recordsjson",
            "records.JSON ",
            "records.",
            "records.txt",
        ] {
            let mut event = private_msg("");
            let Message::PrivateMessage(message) = &mut event else {
                unreachable!()
            };
            message.message = vec![MessageSegment::File {
                data: crate::onebot::FileData {
                    name: name.into(),
                    file: "file_id:opaque-native".into(),
                },
            }];
            assert!(
                trigger.matches(&event).is_none(),
                "unexpected file match: {name}"
            );
        }
        let suffix = Trigger::new(
            TriggerType::Suffix,
            "json",
            "",
            false,
            false,
            Arc::new(TestHandler),
        );
        assert!(suffix.matches(&private_msg("records.JSON")).is_none());
        assert!(suffix.matches(&private_msg("")).is_none());
    }

    fn private_msg(raw_message: &str) -> Message {
        Message::PrivateMessage(PrivateMessage {
            time: 0,
            self_id: 1,
            post_type: "message".to_string(),
            sub_type: "friend".to_string(),
            message_id: 1,
            user_id: 1,
            message: vec![MessageSegment::Text {
                data: TextData {
                    text: raw_message.to_string(),
                },
            }],
            raw_message: raw_message.to_string(),
            font: 0,
            sender: PrivateMessageSender::default(),
        })
    }
}
