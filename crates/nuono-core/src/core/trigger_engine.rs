use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::core::model::Event;

/// 与 gsuid_core 语义兼容的业务触发类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusinessTriggerType {
    Prefix,
    Suffix,
    Keyword,
    FullMatch,
    Command,
    File,
    Regex,
    Message,
}

/// 由业务触发引擎使用的静态触发器定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessTrigger {
    pub id: String,
    pub plugin_name: String,
    pub service_name: String,
    pub trigger_type: BusinessTriggerType,
    pub keyword: String,
    pub prefix: String,
    pub block: bool,
    pub to_me: bool,
    pub priority: i32,
}

impl BusinessTrigger {
    pub fn new(
        id: impl Into<String>,
        trigger_type: BusinessTriggerType,
        keyword: impl Into<String>,
    ) -> Self {
        let id = id.into();
        Self {
            plugin_name: String::new(),
            service_name: String::new(),
            id,
            trigger_type,
            keyword: keyword.into(),
            prefix: String::new(),
            block: false,
            to_me: false,
            priority: 5,
        }
    }

    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn block(mut self, block: bool) -> Self {
        self.block = block;
        self
    }

    pub fn to_me(mut self, to_me: bool) -> Self {
        self.to_me = to_me;
        self
    }

    pub fn plugin(mut self, name: impl Into<String>) -> Self {
        self.plugin_name = name.into();
        self
    }

    pub fn service(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }
}

/// 触发匹配的结果，命令/文本提取已应用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerMatch {
    pub trigger_id: String,
    pub plugin_name: String,
    pub service_name: String,
    pub trigger_type: BusinessTriggerType,
    pub keyword: String,
    pub prefix: String,
    pub command: String,
    pub text: String,
    pub regex_group: Vec<String>,
    pub regex_dict: HashMap<String, String>,
    pub block: bool,
    pub priority: i32,
}

impl TriggerMatch {
    fn from_trigger(trigger: &BusinessTrigger, event: &Event) -> Self {
        let (command, text, regex_group, regex_dict) = extract_command(trigger, event);
        Self {
            trigger_id: trigger.id.clone(),
            plugin_name: trigger.plugin_name.clone(),
            service_name: trigger.service_name.clone(),
            trigger_type: trigger.trigger_type,
            keyword: trigger.keyword.clone(),
            prefix: trigger.prefix.clone(),
            command,
            text,
            regex_group,
            regex_dict,
            block: trigger.block,
            priority: trigger.priority,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TriggerEngine {
    triggers: Vec<BusinessTrigger>,
}

impl TriggerEngine {
    pub fn new() -> Self {
        Self {
            triggers: Vec::new(),
        }
    }

    pub fn with_triggers(triggers: Vec<BusinessTrigger>) -> Self {
        Self { triggers }
    }

    pub fn register(&mut self, trigger: BusinessTrigger) {
        self.triggers.push(trigger);
    }

    pub fn triggers(&self) -> &[BusinessTrigger] {
        &self.triggers
    }

    pub fn match_event(&self, event: &Event) -> Vec<TriggerMatch> {
        let mut matches = self
            .triggers
            .iter()
            .filter(|trigger| check_trigger(trigger, event))
            .map(|trigger| TriggerMatch::from_trigger(trigger, event))
            .collect::<Vec<_>>();

        matches.sort_by(|a, b| {
            let a_has_prefix = !a.prefix.is_empty();
            let b_has_prefix = !b.prefix.is_empty();
            b_has_prefix
                .cmp(&a_has_prefix)
                .then_with(|| a.priority.cmp(&b.priority))
        });

        matches
    }
}

pub fn check_trigger(trigger: &BusinessTrigger, event: &Event) -> bool {
    if trigger.to_me && !event.is_tome {
        return false;
    }

    if trigger.trigger_type == BusinessTriggerType::Message {
        return true;
    }

    if trigger.trigger_type == BusinessTriggerType::File {
        return check_file(&trigger.keyword, event);
    }

    let msg = event.raw_text.as_str();
    if msg.is_empty() {
        return false;
    }

    match trigger.trigger_type {
        BusinessTriggerType::Prefix => {
            msg.starts_with(&format!("{}{}", trigger.prefix, trigger.keyword))
                && !check_fullmatch(&trigger.keyword, &trigger.prefix, msg)
        }
        BusinessTriggerType::Command => {
            let command = format!("{}{}", trigger.prefix, trigger.keyword);
            msg.starts_with(&command) && msg.get(command.len()..).is_some_and(command_boundary_ok)
        }
        BusinessTriggerType::Suffix => {
            msg.starts_with(&trigger.prefix)
                && msg.ends_with(&trigger.keyword)
                && !check_fullmatch(&trigger.keyword, &trigger.prefix, msg)
        }
        BusinessTriggerType::Keyword => {
            msg.contains(&trigger.keyword) && msg.starts_with(&trigger.prefix)
        }
        BusinessTriggerType::FullMatch => check_fullmatch(&trigger.keyword, &trigger.prefix, msg),
        BusinessTriggerType::Regex => {
            regex_matches_with_prefix(&trigger.keyword, &trigger.prefix, msg)
        }
        BusinessTriggerType::File | BusinessTriggerType::Message => unreachable!(),
    }
}

fn command_boundary_ok(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '#' | ':' | ',' | '，' | '：'))
}

fn check_fullmatch(keyword: &str, prefix: &str, msg: &str) -> bool {
    msg == format!("{prefix}{keyword}") && msg.starts_with(prefix)
}

fn check_file(file_type: &str, event: &Event) -> bool {
    event
        .file_name
        .as_deref()
        .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext))
        .map(|ext| ext.eq_ignore_ascii_case(file_type))
        .unwrap_or(false)
}

fn extract_command(
    trigger: &BusinessTrigger,
    event: &Event,
) -> (String, String, Vec<String>, HashMap<String, String>) {
    if trigger.trigger_type != BusinessTriggerType::Regex {
        let mut text = event.raw_text.replacen(&trigger.keyword, "", 1);
        if !trigger.prefix.is_empty() {
            text = text.replacen(&trigger.prefix, "", 1);
        }
        return (trigger.keyword.clone(), text, Vec::new(), HashMap::new());
    }

    let mut text_for_match = event.raw_text.clone();
    if !trigger.prefix.is_empty() {
        text_for_match = text_for_match.replacen(&trigger.prefix, "", 1);
    }

    let Some(regex) = Regex::new(&trigger.keyword).ok() else {
        return (String::new(), text_for_match, Vec::new(), HashMap::new());
    };

    let mut regex_group = Vec::new();
    let mut regex_dict = HashMap::new();
    let mut command = String::new();

    if let Some(captures) = regex.captures(&text_for_match) {
        for capture in captures.iter().skip(1) {
            regex_group.push(capture.map(|m| m.as_str().to_string()).unwrap_or_default());
        }
        command = regex_group.join("|");

        for name in regex.capture_names().flatten() {
            if let Some(value) = captures.name(name) {
                regex_dict.insert(name.to_string(), value.as_str().to_string());
            }
        }
    }

    let parts = regex
        .split(&event.raw_text)
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    (command, parts.join("|"), regex_group, regex_dict)
}

fn regex_matches_with_prefix(pattern: &str, prefix: &str, msg: &str) -> bool {
    if !msg.starts_with(prefix) {
        return false;
    }
    let Ok(regex) = Regex::new(pattern) else {
        return false;
    };
    let text = msg.replacen(prefix, "", 1);
    regex.find(&text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Event, FileSource, MessageReceive, MessageSegment, UserType};

    fn event(raw_text: &str) -> Event {
        Event::from_receive(MessageReceive {
            bot_id: "bot".to_string(),
            bot_self_id: "10000".to_string(),
            user_id: "20000".to_string(),
            user_type: UserType::Group,
            group_id: Some("30000".to_string()),
            content: vec![MessageSegment::Text {
                text: raw_text.to_string(),
            }],
            ..Default::default()
        })
    }

    #[test]
    fn prefix_does_not_match_fullmatch() {
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::Prefix, "help").prefix("/");

        assert!(!check_trigger(&trigger, &event("/help")));
        assert!(check_trigger(&trigger, &event("/help me")));
    }

    #[test]
    fn command_matches_explicit_prefix_and_boundary() {
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::Command, "help").prefix("/");
        let engine = TriggerEngine::with_triggers(vec![trigger]);
        let matches = engine.match_event(&event("/help me"));

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "help");
        assert_eq!(matches[0].text.trim(), "me");
        assert!(engine.match_event(&event("/helpful")).is_empty());
        assert!(engine.match_event(&event("help me")).is_empty());
    }

    #[test]
    fn suffix_does_not_match_fullmatch() {
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::Suffix, "help").prefix("/");

        assert!(!check_trigger(&trigger, &event("/help")));
        assert!(check_trigger(&trigger, &event("/please help")));
    }

    #[test]
    fn keyword_requires_prefix() {
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::Keyword, "help").prefix("/");

        assert!(check_trigger(&trigger, &event("/please help")));
        assert!(!check_trigger(&trigger, &event("please help")));
    }

    #[test]
    fn fullmatch_requires_exact_text() {
        let trigger =
            BusinessTrigger::new("t1", BusinessTriggerType::FullMatch, "help").prefix("/");

        assert!(check_trigger(&trigger, &event("/help")));
        assert!(!check_trigger(&trigger, &event("/help me")));
    }

    #[test]
    fn message_trigger_always_matches_even_empty_text() {
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::Message, "");

        assert!(check_trigger(&trigger, &event("")));
    }

    #[test]
    fn to_me_trigger_requires_tome_event() {
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::Command, "help").to_me(true);
        let mut ev = event("help");
        ev.is_tome = false;
        assert!(!check_trigger(&trigger, &ev));
        ev.is_tome = true;
        assert!(check_trigger(&trigger, &ev));
    }

    #[test]
    fn file_trigger_matches_extension() {
        let ev = Event::from_receive(MessageReceive {
            bot_id: "bot".to_string(),
            user_id: "20000".to_string(),
            content: vec![MessageSegment::File {
                name: "report.zip".to_string(),
                source: FileSource::Url("https://example.com/report.zip".to_string()),
            }],
            ..Default::default()
        });
        let trigger = BusinessTrigger::new("t1", BusinessTriggerType::File, "zip");

        assert!(check_trigger(&trigger, &ev));
    }

    #[test]
    fn file_trigger_matches_bare_native_files_case_insensitively_without_rewriting_names() {
        for pattern in ["json", "JSON"] {
            let trigger = BusinessTrigger::new("files", BusinessTriggerType::File, pattern);
            let engine = TriggerEngine::with_triggers(vec![trigger]);
            for (name, expected) in [
                ("export.JSON", true),
                ("export.JsOn", true),
                ("export.backup.json", true),
                ("export.JSON.exe", false),
                ("export.JSON ", false),
                ("exportjson", false),
            ] {
                let ev = Event::from_receive(MessageReceive {
                    bot_id: "bot".into(),
                    user_id: "native-user".into(),
                    content: vec![MessageSegment::File {
                        name: name.into(),
                        source: FileSource::Id("opaque-native".into()),
                    }],
                    ..Default::default()
                });
                assert!(ev.raw_text.is_empty());
                assert_eq!(
                    !engine.match_event(&ev).is_empty(),
                    expected,
                    "extension: {name}"
                );
                assert_eq!(ev.file_name.as_deref(), Some(name));
            }
        }
        let trigger = BusinessTrigger::new("files", BusinessTriggerType::File, "json");
        assert!(!check_trigger(&trigger, &event("export.JSON")));
    }

    #[test]
    fn regex_extracts_groups_and_named_groups() {
        let trigger =
            BusinessTrigger::new("t1", BusinessTriggerType::Regex, r"^uid\s+(?P<uid>\d+)$")
                .prefix("/");
        let matches =
            TriggerEngine::with_triggers(vec![trigger]).match_event(&event("/uid 123456"));

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].regex_group, vec!["123456"]);
        assert_eq!(
            matches[0].regex_dict.get("uid").map(String::as_str),
            Some("123456")
        );
        assert_eq!(matches[0].command, "123456");
    }

    #[test]
    fn sorting_prefers_prefixed_then_priority() {
        let t1 =
            BusinessTrigger::new("no-prefix", BusinessTriggerType::Command, "help").priority(1);
        let t2 = BusinessTrigger::new("prefix", BusinessTriggerType::Command, "help")
            .prefix("/")
            .priority(5);
        let t3 = BusinessTrigger::new("prefix-high", BusinessTriggerType::Command, "help")
            .prefix("/")
            .priority(1);

        let matches =
            TriggerEngine::with_triggers(vec![t1, t2, t3]).match_event(&event("/help me"));
        let ids = matches
            .into_iter()
            .map(|m| m.trigger_id)
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["prefix-high", "prefix"]);
    }
}
