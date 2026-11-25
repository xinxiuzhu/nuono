//! 与具体服务商解耦的模型调用、向量化（Embedding）及受限视觉内容定义。
//!
//! 具体的 HTTP 传输协议、数据库配置读取和宿主生命周期由上层调用方负责实现。
pub mod embedding;
pub mod media;
pub mod tools;
pub mod usage;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// AI 错误类型
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("AI 服务请求失败: {0}")]
    RequestFailed(String),

    #[error("AI 服务响应解析失败: {0}")]
    ResponseParseFailed(String),

    #[error("AI 服务超时: {0}")]
    Timeout(String),

    #[error("AI 配额耗尽: {0}")]
    RateLimitExceeded(String),

    #[error("AI API Key 无效")]
    InvalidApiKey,

    #[error("模型不存在: {0}")]
    ModelNotFound(String),

    #[error("上下文长度超限: {0} > {1}")]
    ContextLengthExceeded(usize, usize),

    #[error("{0}")]
    Other(String),
}

/// 聊天消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    /// 用户输入或工具返回的图片（HTTPS 链接或 Data URI），绝不使用本地文件路径。
    /// 服务商适配器会保留工具来源标记，宿主不会将工具返回的图片当作用户指令。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
    /// 服务商透传的上下文续接数据（例如加密的推理过程数据）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            images: Vec::new(),
            name: None,
            tool_calls: Vec::new(),
            continuation: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            images: Vec::new(),
            name: None,
            tool_calls: Vec::new(),
            continuation: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            images: Vec::new(),
            name: None,
            tool_calls: Vec::new(),
            continuation: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = images;
        self
    }

    pub fn with_continuation(mut self, items: Vec<Value>) -> Self {
        self.continuation = items;
        self
    }

    pub fn with_tool_calls(mut self, calls: Vec<ChatToolCall>) -> Self {
        self.tool_calls = calls;
        self
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            images: Vec::new(),
            name: None,
            tool_calls: Vec::new(),
            continuation: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// 与具体服务商无关的模型工具/函数调用定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTool {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

impl ChatTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters_schema,
        }
    }
}

/// 聊天模型返回的工具调用请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

/// 聊天请求
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
}

impl ChatRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            ..Default::default()
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_max_tokens(mut self, tokens: u32) -> Self {
        self.max_tokens = Some(tokens);
        self
    }

    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.messages.insert(0, ChatMessage::system(prompt));
        self
    }

    pub fn push(mut self, message: ChatMessage) -> Self {
        self.messages.push(message);
        self
    }

    pub fn with_tools(mut self, tools: Vec<ChatTool>) -> Self {
        self.tools = tools;
        self
    }
}

/// 聊天响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub content: String,
    pub usage: UsageInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
    /// 服务商透传的上下文续接数据（例如加密的推理过程数据）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation: Vec<Value>,
}

/// Token 用量信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// AI 服务 trait — 定义统一的 LLM 调用接口
#[async_trait]
pub trait AiService: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AiError>;

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChatStreamChunk, AiError>>, AiError>;

    fn count_tokens(&self, text: &str) -> usize {
        let chinese = text.chars().filter(|c| !c.is_ascii()).count();
        let english = text.chars().filter(|c| c.is_ascii()).count();
        (english as f32 * 0.75 + chinese as f32 * 1.5) as usize
    }

    fn service_name(&self) -> &str;
}

/// 流式聊天响应块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatStreamChunk {
    pub id: String,
    pub model: String,
    pub delta_content: String,
    pub finish_reason: Option<String>,
}

/// 模型信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_length: usize,
    pub supports_streaming: bool,
    pub supports_function_calling: bool,
}

/// AI 服务配置
#[derive(Clone, Serialize, Deserialize)]
pub struct AiConfig {
    pub provider: String,
    pub provider_type: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_context_length")]
    pub context_length: usize,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 接口地址、系统提示词和自定义扩展字段中也可能包含敏感凭据，因此不在 Debug 中输出。
        f.debug_struct("AiConfig")
            .field("provider", &self.provider)
            .field("provider_type", &self.provider_type)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("context_length", &self.context_length)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

fn default_max_tokens() -> u32 {
    2048
}
fn default_temperature() -> f32 {
    0.7
}
fn default_context_length() -> usize {
    128000
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            provider_type: "openai_compatible".into(),
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            max_tokens: 2048,
            temperature: 0.7,
            context_length: 128000,
            system_prompt: None,
            extra: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChatMessage;

    #[test]
    fn checkpoints_without_images_remain_compatible() {
        let legacy = serde_json::json!({"role": "user", "content": "原有会话"});
        let message: ChatMessage = serde_json::from_value(legacy).unwrap();
        assert!(message.images.is_empty());
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("images")
                .is_none()
        );

        let message =
            ChatMessage::user("").with_images(vec!["https://example.com/image.png".into()]);
        let saved = serde_json::to_value(&message).unwrap();
        let restored: ChatMessage = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.images, message.images);
        assert!(restored.content.is_empty());
    }
}
