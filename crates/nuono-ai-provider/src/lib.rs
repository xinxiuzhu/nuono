//! 通用 AI 接口的 HTTP 传输层实现。
//! 数据库查询、凭据管理与异步任务生命周期仍由宿主负责。
pub mod embedding;
pub mod llm_provider;
pub mod usage;

pub use llm_provider::{OpenAiCompatibleProvider, ProviderType, parse_provider_type};
