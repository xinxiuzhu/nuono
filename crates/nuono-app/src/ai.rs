//! 应用层会话历史管理与模型路由匹配。
//! 同时重导出 `nuono_ai` 的基础模型接口，保持对外引用路径一致。
pub use nuono_ai::*;
use std::collections::HashMap;

/// 对话历史管理器
#[derive(Debug, Clone)]
pub struct ConversationManager {
    conversations: HashMap<String, Vec<ChatMessage>>,
    pub max_rounds: usize,
    pub system_prompt: Option<String>,
}

impl ConversationManager {
    pub fn new(max_rounds: usize) -> Self {
        Self {
            conversations: HashMap::new(),
            max_rounds,
            system_prompt: None,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn add_user_message(&mut self, session_id: &str, content: impl Into<String>) {
        let messages = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        messages.push(ChatMessage::user(content));
        self.trim_history(session_id);
    }

    pub fn add_assistant_message(&mut self, session_id: &str, content: impl Into<String>) {
        let messages = self
            .conversations
            .entry(session_id.to_string())
            .or_default();
        messages.push(ChatMessage::assistant(content));
        self.trim_history(session_id);
    }

    pub fn get_messages(&self, session_id: &str) -> Vec<ChatMessage> {
        let mut result = Vec::new();
        if let Some(ref prompt) = self.system_prompt {
            result.push(ChatMessage::system(prompt));
        }
        if let Some(messages) = self.conversations.get(session_id) {
            result.extend(messages.clone());
        }
        result
    }

    pub fn clear(&mut self, session_id: &str) {
        self.conversations.remove(session_id);
    }

    fn trim_history(&mut self, session_id: &str) {
        if let Some(messages) = self.conversations.get_mut(session_id) {
            let max_messages = self.max_rounds * 2;
            if messages.len() > max_messages {
                let drain_count = messages.len() - max_messages;
                messages.drain(0..drain_count);
            }
        }
    }

    pub fn conversation_count(&self) -> usize {
        self.conversations.len()
    }
}

impl Default for ConversationManager {
    fn default() -> Self {
        Self::new(10)
    }
}

/// 判断模型是否匹配路由引用（同时支持 `id:<编号>` 精确引用与忽略大小写的名称、模型名或提供方别名）。
pub fn model_route_matches(
    id: i64,
    name: &str,
    model: &str,
    provider: &str,
    reference: &str,
) -> bool {
    let reference = reference.trim();
    if let Some(id_ref) = reference.strip_prefix("id:") {
        return id_ref.parse::<i64>().ok() == Some(id);
    }
    name.eq_ignore_ascii_case(reference)
        || model.eq_ignore_ascii_case(reference)
        || provider.eq_ignore_ascii_case(reference)
}

#[cfg(test)]
mod route_tests {
    use super::model_route_matches;

    #[test]
    fn stable_identity_survives_rename_without_alias_collisions() {
        assert!(model_route_matches(
            12, "renamed", "model", "vendor", "id:12"
        ));
        assert!(!model_route_matches(13, "id:12", "id:12", "id:12", "id:12"));
        assert!(!model_route_matches(12, "x", "x", "x", "id:bad"));
        assert!(model_route_matches(
            12,
            "Primary",
            "model",
            "vendor",
            " primary "
        ));
    }
}
