use super::Event;
use regex::Regex;

/// 显式声明无命令前缀消息的监听范围。
///
/// 文本正则与消息段类型按“任一匹配”判断。宿主只在无显式前缀归属、监听开关开启、
/// 插件权限和群级策略通过时调用 `Plugin::on_message_listen`，事件保留原文和消息段。
/// 正则在加载时编译一次；没有作用域的声明会被拒绝，不会隐式监听全部消息。
///
/// ```
/// use nuono_plugin::{Event, PluginMessageListener};
/// let listener = PluginMessageListener::new("链接与分享卡片")
///     .with_text_pattern(r"(?i)https?://").unwrap()
///     .with_segment_types(["json"]);
/// let raw = serde_json::json!({"post_type":"message", "raw_message":"https://example.com"});
/// assert!(listener.matches(&Event::new(&raw)));
/// ```
#[derive(Debug, Clone)]
pub struct PluginMessageListener {
    description: String,
    text_pattern: Option<Regex>,
    segment_types: Vec<String>,
}

impl PluginMessageListener {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            text_pattern: None,
            segment_types: Vec::new(),
        }
    }

    pub fn with_text_pattern(mut self, pattern: &str) -> Result<Self, regex::Error> {
        self.text_pattern = Some(Regex::new(pattern)?);
        Ok(self)
    }

    pub fn with_segment_types(
        mut self,
        types: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.segment_types = types.into_iter().map(Into::into).collect();
        self.segment_types.sort();
        self.segment_types.dedup();
        self
    }

    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn text_pattern(&self) -> Option<&str> {
        self.text_pattern.as_ref().map(Regex::as_str)
    }
    pub fn segment_types(&self) -> &[String] {
        &self.segment_types
    }

    /// 校验加载时的声明。至少一个匹配范围，消息段类型不得为空。
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.description.trim().is_empty()
            || (self.text_pattern.is_none() && self.segment_types.is_empty())
            || self
                .text_pattern
                .as_ref()
                .is_some_and(|pattern| pattern.as_str().trim().is_empty())
            || self.segment_types.iter().any(|kind| kind.trim().is_empty())
        {
            return Err(crate::error::NuonoError::Validation(
                "自动监听需要说明及非空匹配范围".into(),
            ));
        }
        Ok(())
    }

    pub fn matches(&self, event: &Event<'_>) -> bool {
        event.post_type() == "message"
            && (self
                .text_pattern
                .as_ref()
                .is_some_and(|pattern| pattern.is_match(event.text()))
                || event
                    .message()
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|segments| {
                        segments.iter().any(|segment| {
                            segment
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|kind| {
                                    self.segment_types.iter().any(|declared| declared == kind)
                                })
                        })
                    }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_scope_matches_text_or_segment_but_never_notices() {
        let listener = PluginMessageListener::new("链接及分享卡片")
            .with_text_pattern(r"(?i)https?://")
            .unwrap()
            .with_segment_types(["json", "json"]);
        listener.validate().unwrap();
        assert_eq!(listener.segment_types(), &["json"]);
        for raw in [
            json!({"post_type":"message","raw_message":"HTTPS://example.com"}),
            json!({"post_type":"message","message":[{"type":"json","data":{"data":"{}"}}]}),
        ] {
            assert!(listener.matches(&Event::new(&raw)));
        }
        for raw in [
            json!({"post_type":"notice","raw_message":"https://example.com"}),
            json!({"post_type":"message","raw_message":"帮助","message":[{"type":"image"}]}),
        ] {
            assert!(!listener.matches(&Event::new(&raw)));
        }
    }

    #[test]
    fn invalid_or_implicit_scopes_are_rejected() {
        assert!(
            PluginMessageListener::new("空正则")
                .with_text_pattern("")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(PluginMessageListener::new("隐式全量").validate().is_err());
        assert!(
            PluginMessageListener::new("")
                .with_segment_types(["json"])
                .validate()
                .is_err()
        );
        assert!(
            PluginMessageListener::new("空类型")
                .with_segment_types([""])
                .validate()
                .is_err()
        );
        assert!(
            PluginMessageListener::new("坏正则")
                .with_text_pattern("[")
                .is_err()
        );
    }
}
