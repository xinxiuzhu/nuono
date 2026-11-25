//! 模型服务商返回的 Token 用量统计。若服务商未返回某项指标则保持为 `None`，不做本地估算。
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// 已包含在 `input_tokens` 中，不重复累加。
    pub cached_input_tokens: Option<u64>,
    /// 已包含在 `output_tokens` 中，不会重复累加到 `total_tokens`。
    pub reasoning_output_tokens: Option<u64>,
}
impl ReportedUsage {
    pub fn is_reported(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.total_tokens.is_some()
            || self.cached_input_tokens.is_some()
            || self.reasoning_output_tokens.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageContext {
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    pub model_id: Option<i64>,
    pub name: String,
    pub provider: String,
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOperation {
    Chat,
    ChatStream,
    Embedding,
}
impl UsageOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::ChatStream => "chat_stream",
            Self::Embedding => "embedding",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOutcome {
    Succeeded,
    Failed,
    Cancelled,
}
impl UsageOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 单次实际出站模型请求的用量记录，不包含消息正文、请求 URL、API 密钥或错误详情。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub request_id: String,
    pub started_at_ms: u64,
    pub elapsed_ms: u64,
    pub context: UsageContext,
    pub model: String,
    pub operation: UsageOperation,
    pub outcome: UsageOutcome,
    pub usage: ReportedUsage,
}

/// 轻量、非阻塞的用量统计回调接口，具体的持久化落库与优雅停机由宿主负责。
pub trait UsageObserver: Send + Sync {
    fn observe(&self, event: UsageEvent);
}
