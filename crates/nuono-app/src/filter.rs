//! 入站消息过滤策略（黑名单、关键词与正则过滤）及存储层抽象接口。

use nuono_error::{NuonoError, Result};
use regex::Regex;
use serde_json::Value;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterResponse {
    Continue,
    Stop,
}

pub struct FilterHandler {
    db: std::sync::Arc<dyn FilterStore>,
}

impl FilterHandler {
    pub fn new(db: impl FilterStore + 'static) -> Self {
        Self {
            db: std::sync::Arc::new(db),
        }
    }

    fn check_logic_match(&self, text: &str, pattern: &str) -> bool {
        if pattern.contains('+') {
            pattern.split('+').all(|part| text.contains(part.trim()))
        } else if pattern.contains('|') {
            pattern.split('|').any(|part| text.contains(part.trim()))
        } else {
            text.contains(pattern)
        }
    }

    fn apply_prefix_protection(&self, text: String) -> String {
        let protected_prefixes = ["rt", "/"];
        for prefix in protected_prefixes {
            if text.starts_with(prefix) {
                return format!("[禁止诱导触发] {}", text);
            }
        }
        text
    }

    /// 供事件运行时调用的过滤入口：当过滤评估出现内部错误时，统一按拦截（`Stop`）处理以确保安全
    /// （避免外层运行时将 `Err` 当成非致命错误继续放行）。
    pub async fn handle_event(&self, event: &Value) -> Result<FilterResponse> {
        match self.evaluate_event(event).await {
            Ok(response) => Ok(response),
            Err(err) => {
                error!(error = %err, "Security filter evaluation failed; blocking event");
                Ok(FilterResponse::Stop)
            }
        }
    }

    /// 执行消息过滤规则评估，将明确的规则拦截（`Ok(FilterResponse::Stop)`）与底层数据库/正则配置错误（`Err`）区分开。
    pub async fn evaluate_event(&self, event: &Value) -> Result<FilterResponse> {
        if event.get("post_type").and_then(|t| t.as_str()) != Some("message") {
            return Ok(FilterResponse::Continue);
        }

        let bot_id = identity_string(event, "self_id")?;
        let user_id = identity_string(event, "user_id")?;
        let group_id = optional_identity_string(event, "group_id")?;
        let raw_text = event
            .get("raw_message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        let message_text = self.apply_prefix_protection(raw_text);

        if self.db.is_blacklisted("user", &user_id).await? {
            warn!("Blocked blacklisted user: {}", user_id);
            let _ = self
                .db
                .add_log("WARN", &bot_id, &format!("🚫 拦截黑名单用户: {}", user_id))
                .await;
            return Ok(FilterResponse::Stop);
        }

        if let Some(gid) = &group_id {
            if self.db.is_blacklisted("group", gid).await? {
                warn!("Blocked blacklisted group: {}", gid);
                let _ = self
                    .db
                    .add_log("WARN", &bot_id, &format!("🚫 拦截黑名单群: {}", gid))
                    .await;
                return Ok(FilterResponse::Stop);
            }

            // 其他平台已由运行时使用完整 ConversationRef 校验，不能用裸群号查询 OneBot 对象。
            if event
                .get("platform")
                .and_then(Value::as_str)
                .is_none_or(|platform| platform == "onebot")
                && !self.db.is_group_enabled(gid).await?
            {
                warn!("Blocked disabled or expired group: {}", gid);
                let _ = self
                    .db
                    .add_log("WARN", &bot_id, &format!("🚫 拦截已禁用/过期群: {}", gid))
                    .await;
                return Ok(FilterResponse::Stop);
            }
        }

        let rules = self.db.get_filter_rules("receive").await?;
        for rule in rules {
            let pattern = rule.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let mode = rule
                .get("match_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("simple");

            let matched = match mode {
                "regex" => Regex::new(pattern)?.is_match(&message_text),
                "logic" => self.check_logic_match(&message_text, pattern),
                _ => message_text.contains(pattern),
            };

            if matched {
                info!("Message filtered by rule: {}", pattern);
                let _ = self
                    .db
                    .add_log(
                        "INFO",
                        &bot_id,
                        &format!("🔇 消息被过滤规则拦截: {}", pattern),
                    )
                    .await;
                return Ok(FilterResponse::Stop);
            }
        }

        Ok(FilterResponse::Continue)
    }
}

fn identity_string(event: &Value, key: &str) -> Result<String> {
    optional_identity_string(event, key)?.ok_or_else(|| {
        NuonoError::EventProcessing(format!("message event is missing identity field {key}"))
    })
}

fn optional_identity_string(event: &Value, key: &str) -> Result<Option<String>> {
    match event.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::Number(value)) => Ok(Some(value.to_string())),
        Some(Value::String(_)) => Err(NuonoError::EventProcessing(format!(
            "identity field {key} cannot be empty"
        ))),
        Some(_) => Err(NuonoError::EventProcessing(format!(
            "identity field {key} must be a number or string"
        ))),
    }
}

#[async_trait::async_trait]
pub trait FilterStore: Send + Sync {
    async fn is_blacklisted(&self, kind: &str, id: &str) -> Result<bool>;
    async fn is_group_enabled(&self, group: &str) -> Result<bool>;
    async fn get_filter_rules(&self, kind: &str) -> Result<Vec<Value>>;
    async fn add_log(&self, level: &str, bot: &str, message: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filter_identity_accepts_native_string_ids() {
        let event = serde_json::json!({
            "self_id": "cli_app",
            "user_id": "ou_user",
            "group_id": "oc_chat"
        });

        assert_eq!(identity_string(&event, "self_id").unwrap(), "cli_app");
        assert_eq!(identity_string(&event, "user_id").unwrap(), "ou_user");
        assert_eq!(
            optional_identity_string(&event, "group_id")
                .unwrap()
                .as_deref(),
            Some("oc_chat")
        );
    }
}
