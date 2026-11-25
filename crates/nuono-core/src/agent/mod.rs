//! 应用层 Agent 用例的宿主运行时装配适配器。
pub(crate) mod output;
mod replies;
pub mod tool;
use crate::{config::ConfigManager, db::Database};
#[derive(Debug, Clone)]
pub struct AgentResponse {
    pub action: serde_json::Value,
}
impl AgentResponse {
    pub fn into_action_string(self) -> String {
        self.action.to_string()
    }
}
pub use nuono_app::agent::{AgentContext, AgentPolicy, AutonomyLevel};
use std::sync::Arc;
pub use tool::{
    AgentTool, AgentToolContext, BuiltinToolRegistry, ToolExecution, ToolExecutionStatus,
    ToolParameter, ToolParameterType, ToolRegistry, ToolResource, ToolResourceMode, ToolRiskLevel,
    ToolScheduling,
};
#[async_trait::async_trait]
impl nuono_app::agent::ports::AgentConfiguration for ConfigManager {
    async fn ai_settings(&self) -> crate::config::AIConfig {
        self.snapshot().await.ai.clone()
    }
}
pub struct BuiltinModelProviderFactory {
    db: Database,
}
impl nuono_app::agent::ports::ModelProviderFactory for BuiltinModelProviderFactory {
    fn create(
        &self,
        config: &crate::ai::AiConfig,
    ) -> Result<Arc<dyn crate::ai::AiService>, crate::ai::AiError> {
        crate::ai::usage_tracking::from_config(config, &self.db)
            .map(|provider| Arc::new(provider) as Arc<dyn crate::ai::AiService>)
    }
}
/// 宿主装配门面；通过显式委托提供应用能力，避免隐式暴露内部 runtime 的全部 API。
#[derive(Clone)]
pub struct AgentRuntime {
    inner: nuono_app::agent::runtime::AgentRuntime,
    replies: Arc<replies::ReplyTasks>,
}
impl AgentRuntime {
    pub(crate) async fn spawn_settling_reply<F, Fut>(&self, reply: F) -> crate::error::Result<()>
    where
        F: FnOnce(tokio::sync::watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.replies.spawn_settling(reply).await
    }

    pub(crate) async fn spawn_reply(
        &self,
        reply: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> crate::error::Result<()> {
        self.replies.spawn(reply).await
    }

    /// 收到停机信号时立即通知取消，无需等待在途 HTTP 请求排空。
    pub fn request_shutdown(&self) {
        self.replies.request_shutdown();
        self.inner.request_shutdown();
    }

    /// 在插件与数据库关闭前，等待所有在途 Agent 任务执行与回复投递收尾。
    pub async fn shutdown(&self) -> crate::error::Result<()> {
        self.request_shutdown();
        let (execution, ()) = tokio::join!(self.inner.shutdown(), self.replies.shutdown());
        execution
    }

    pub async fn wait_for_idle(&self) -> crate::error::Result<()> {
        self.inner.wait_for_idle().await
    }

    pub fn begin_conversation(&self) -> nuono_app::agent::memory::MemoryConversationGuard {
        self.inner.begin_conversation()
    }
    pub async fn maintain_memories(
        &self,
        require_idle: bool,
    ) -> crate::error::Result<nuono_app::agent::memory::MemoryMaintenanceResult> {
        let indexed = match self.inner.maintain_semantic_memories(require_idle).await {
            Ok(count) => count,
            Err(error) if !require_idle => return Err(error),
            Err(_) => {
                tracing::warn!("记忆语义索引预热失败，稍后重试");
                0
            }
        };
        let mut result = self.inner.maintain_memories(require_idle).await?;
        result.indexed_memories = indexed;
        if indexed > 0 && result.status != "organized" {
            result.status = "indexed".into();
        }
        Ok(result)
    }
    pub(crate) async fn prepare_companion_memory(
        &self,
        ctx: &AgentContext,
        phase: nuono_app::agent::companion_memory::MemoryPhase,
    ) -> crate::error::Result<nuono_app::agent::companion_memory::MemoryPlan> {
        self.inner.prepare_companion_memory(ctx, phase).await
    }
    pub(crate) async fn compress_companion_history(
        &self,
        ctx: &AgentContext,
        experiences: &[nuono_app::agent::companion_memory::CompanionExperience],
    ) -> crate::error::Result<nuono_app::agent::companion_memory::CompanionSummary> {
        self.inner
            .compress_companion_history(ctx, experiences)
            .await
    }
    pub async fn submit_text(
        &self,
        ctx: AgentContext,
    ) -> crate::error::Result<Option<nuono_app::agent::run::AgentRunTask>> {
        self.inner.submit_text(ctx).await
    }
    pub async fn submit_text_with_output(
        &self,
        ctx: AgentContext,
        progress: tokio::sync::watch::Sender<nuono_app::agent::output::OutputProgress>,
    ) -> crate::error::Result<Option<nuono_app::agent::run::AgentRunTask>> {
        self.inner.submit_text_with_output(ctx, progress).await
    }
    pub async fn output_policy(
        &self,
        ctx: &AgentContext,
    ) -> nuono_app::agent::output::OutputPolicy {
        self.inner.output_policy(ctx).await
    }
    pub async fn resume_approval(
        &self,
        id: i64,
    ) -> crate::error::Result<Option<nuono_app::agent::run::AgentRunReply>> {
        self.inner.resume_approval(id).await
    }
    pub async fn collect_children(
        &self,
        id: &str,
    ) -> crate::error::Result<Option<nuono_app::agent::run::AgentRunReply>> {
        self.inner.collect_children(id).await
    }
    pub async fn list_runs(
        &self,
        status: Option<&str>,
        limit: u64,
    ) -> crate::error::Result<Vec<nuono_app::agent::run::AgentRunSummary>> {
        self.inner.list_runs(status, limit).await
    }
    pub async fn get_run(
        &self,
        id: &str,
    ) -> crate::error::Result<Option<nuono_app::agent::run::AgentRunView>> {
        self.inner.get_run(id).await
    }
    pub async fn steer_run(&self, id: &str, text: &str) -> crate::error::Result<()> {
        self.inner.steer_run(id, text).await
    }
    pub async fn cancel_run(&self, id: &str) -> crate::error::Result<()> {
        self.inner.cancel_run(id).await
    }
    pub fn notify_activity(&self) {
        self.inner.notify_activity();
    }
    pub fn autonomy_level(&self) -> AutonomyLevel {
        self.inner.autonomy_level()
    }

    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.inner.tool_registry()
    }

    pub async fn handle_text(
        &self,
        ctx: AgentContext,
    ) -> Option<nuono_app::agent::runtime::AgentResponse> {
        self.inner.handle_text(ctx).await
    }

    pub async fn handle_application_event(
        &self,
        bot_id: i64,
        event: &serde_json::Value,
    ) -> Option<nuono_app::agent::runtime::AgentResponse> {
        self.handle_text(AgentContext::from_runtime_event(bot_id, event)?)
            .await
    }
    pub async fn handle_runtime_event(
        &self,
        bot_id: i64,
        event: &serde_json::Value,
    ) -> Option<AgentResponse> {
        let response = self.handle_application_event(bot_id, event).await?;
        match crate::plugin::PluginResponse::text(event, response.content) {
            crate::plugin::PluginResponse::Reply(action) => Some(AgentResponse { action }),
            _ => None,
        }
    }
    pub fn new(db: Database, config: Arc<ConfigManager>) -> Self {
        Self::with_tool_registry(db, config, BuiltinToolRegistry::reply_only())
    }
    pub fn with_tool_registry(
        db: Database,
        config: Arc<ConfigManager>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        let embeddings = crate::ai::memory_embeddings::for_memory(db.clone());
        Self {
            replies: Arc::new(replies::ReplyTasks::default()),
            inner: nuono_app::agent::runtime::AgentRuntime::with_ports(
                Arc::new(db.clone()),
                config,
                Arc::new(BuiltinModelProviderFactory { db }),
                registry,
            )
            .with_memory_embeddings(embeddings),
        }
    }
}
