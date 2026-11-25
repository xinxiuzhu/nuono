//! Agent 运行时依赖的外部配置与模型提供方工厂接口。
use crate::ai::{AiConfig, AiError, AiService};
use crate::settings::AIConfig;
use std::sync::Arc;
#[async_trait::async_trait]
pub trait AgentConfiguration: Send + Sync {
    async fn ai_settings(&self) -> AIConfig;
}
pub trait ModelProviderFactory: Send + Sync {
    fn create(&self, config: &AiConfig) -> Result<Arc<dyn AiService>, AiError>;
}
