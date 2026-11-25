//! Agent 存储层适配：将数据库实体模型转换为应用层 DTO。
use crate::Database;
use crate::error::Result;
use nuono_app::agent::memory as nuono_memory;
use nuono_app::agent::store::*;

impl From<crate::entity::ai_models::Model> for ProviderModel {
    fn from(row: crate::entity::ai_models::Model) -> Self {
        Self {
            id: row.id,
            name: row.name,
            provider: row.provider,
            provider_type: row.provider_type,
            model_id: row.model_id,
            api_key: row.api_key,
            base_url: row.base_url,
            is_default: row.is_default,
            enabled: row.enabled,
            created_at: crate::clock::load_time(row.created_at),
            updated_at: crate::clock::load_time(row.updated_at),
        }
    }
}
impl From<crate::conversation_store::ConversationMessageRow> for ConversationEntry {
    fn from(row: crate::conversation_store::ConversationMessageRow) -> Self {
        Self {
            id: row.id,
            session_id: row.session_id,
            role: row.role,
            content: row.content,
            timestamp: crate::clock::load_time(row.timestamp),
        }
    }
}
impl From<crate::entity::agent_memories::Model> for MemoryEntry {
    fn from(row: crate::entity::agent_memories::Model) -> Self {
        Self {
            id: row.id,
            scope_type: row.scope_type,
            scope_key: row.scope_key,
            memory_key: row.memory_key,
            content: row.content,
            tags_json: row.tags_json,
            enabled: row.enabled,
            expires_at: crate::clock::load_time(row.expires_at),
            created_by: row.created_by,
            updated_by: row.updated_by,
            created_at: crate::clock::load_time(row.created_at),
            updated_at: crate::clock::load_time(row.updated_at),
        }
    }
}
impl From<crate::entity::agent_tool_approvals::Model> for ToolApproval {
    fn from(row: crate::entity::agent_tool_approvals::Model) -> Self {
        Self {
            run_id: row.run_id,
            context_json: row.context_json,
            execution_json: row.execution_json,
            id: row.id,
            session_id: row.session_id,
            bot_id: row.bot_id,
            platform: row.platform,
            message_type: row.message_type,
            user_id: row.user_id,
            group_id: row.group_id,
            channel_id: row.channel_id,
            message_id: row.message_id,
            tool_name: row.tool_name,
            risk_level: row.risk_level,
            status: row.status,
            input_json: row.input_json,
            reason: row.reason,
            decision_by: row.decision_by,
            decision_reason: row.decision_reason,
            decided_at: crate::clock::load_time(row.decided_at),
            created_at: crate::clock::load_time(row.created_at),
        }
    }
}
#[async_trait::async_trait]
impl AgentStore for Database {
    async fn resolve_ai_persona(
        &self,
        key: &nuono_app::persona::PersonaBindingKey,
    ) -> Result<Option<nuono_app::persona::PersonaRecord>> {
        Database::resolve_ai_persona(self, key).await
    }
    async fn create_agent_children(
        &self,
        parent: AgentRunRecord,
        revision: i64,
        children: Vec<AgentRunRecord>,
    ) -> Result<bool> {
        self.create_agent_children(parent, revision, children).await
    }
    async fn claim_agent_child(
        &self,
        child: AgentRunRecord,
        expected_revision: i64,
        parent_revision: i64,
    ) -> Result<bool> {
        Database::claim_agent_child(self, child, expected_revision, parent_revision).await
    }
    async fn list_agent_children(&self, parent_id: &str) -> Result<Vec<AgentRunRecord>> {
        self.list_agent_children(parent_id).await
    }
    async fn create_agent_run(&self, run: AgentRunRecord) -> Result<()> {
        Database::create_agent_run(self, run).await
    }
    async fn update_agent_run(&self, run: AgentRunRecord, expected_revision: i64) -> Result<bool> {
        Database::update_agent_run(self, run, expected_revision).await
    }
    async fn interrupt_agent_run(
        &self,
        id: &str,
        expected_revision: i64,
        reason: &str,
    ) -> Result<bool> {
        Database::interrupt_agent_run(self, id, expected_revision, reason).await
    }
    async fn reconcile_agent_runs_before_start(
        &self,
        reason: &str,
    ) -> Result<AgentRunRecoverySummary> {
        Database::reconcile_agent_runs_before_start(self, reason).await
    }
    async fn get_agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>> {
        Database::get_agent_run(self, id).await
    }
    async fn list_agent_runs(
        &self,
        status: Option<&str>,
        limit: u64,
    ) -> Result<Vec<AgentRunListRecord>> {
        Database::list_agent_runs(self, status, limit).await
    }
    async fn active_agent_run(&self, session_id: &str) -> Result<Option<AgentRunRecord>> {
        Database::active_agent_run(self, session_id).await
    }
    async fn agent_run_for_approval(&self, id: i64) -> Result<Option<AgentRunRecord>> {
        Database::agent_run_for_approval(self, id).await
    }
    async fn complete_agent_tool_approval(
        &self,
        id: i64,
        status: &str,
        execution_json: String,
    ) -> Result<Option<ToolApproval>> {
        Database::complete_agent_tool_approval(self, id, status, execution_json)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn list_ai_models(&self) -> Result<Vec<ProviderModel>> {
        Database::list_ai_models(self)
            .await
            .map(|rows| rows.into_iter().map(Into::into).collect())
    }
    async fn get_default_ai_model(&self) -> Result<Option<ProviderModel>> {
        Database::get_default_ai_model(self)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn recent_ai_conversations(
        &self,
        session_id: &str,
        limit: u64,
    ) -> Result<Vec<ConversationEntry>> {
        Database::recent_ai_conversations(self, session_id, limit)
            .await
            .map(|rows| rows.into_iter().map(Into::into).collect())
    }
    async fn save_ai_conversation(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        Database::save_ai_conversation(self, session_id, role, content).await
    }
    async fn save_agent_tool_receipt(&self, receipt: AgentToolReceiptRecord) -> Result<()> {
        Database::save_agent_tool_receipt(self, receipt).await
    }
    async fn create_agent_tool_approval(&self, approval: AgentToolApprovalRecord) -> Result<i64> {
        Database::create_agent_tool_approval(self, approval).await
    }
    async fn decide_agent_tool_approval(
        &self,
        id: i64,
        approved: bool,
        actor: Option<String>,
        reason: Option<String>,
    ) -> Result<Option<ToolApproval>> {
        Database::decide_agent_tool_approval(self, id, approved, actor, reason)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn claim_agent_tool_approval_for_execution(
        &self,
        id: i64,
    ) -> Result<Option<ToolApproval>> {
        Database::claim_agent_tool_approval_for_execution(self, id)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn get_agent_tool_approval(&self, id: i64) -> Result<Option<ToolApproval>> {
        Database::get_agent_tool_approval(self, id)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn mark_agent_tool_approval_status(
        &self,
        id: i64,
        status: &str,
    ) -> Result<Option<ToolApproval>> {
        Database::mark_agent_tool_approval_status(self, id, status)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn list_agent_memories(
        &self,
        scope_type: Option<&str>,
        scope_key: Option<&str>,
        enabled: Option<bool>,
        active_only: bool,
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        Database::list_agent_memories(self, scope_type, scope_key, enabled, active_only, limit)
            .await
            .map(|rows| rows.into_iter().map(Into::into).collect())
    }
    async fn list_agent_memories_for_scopes(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        Database::list_agent_memories_for_scopes(self, scopes, limit)
            .await
            .map(|rows| rows.into_iter().map(Into::into).collect())
    }
    async fn upsert_agent_memory(&self, record: AgentMemoryRecord) -> Result<(MemoryEntry, bool)> {
        Database::upsert_agent_memory(self, record)
            .await
            .map(|(row, created)| (row.into(), created))
    }
    async fn update_agent_memory(
        &self,
        id: i64,
        record: AgentMemoryUpdateRecord,
    ) -> Result<Option<MemoryEntry>> {
        Database::update_agent_memory(self, id, record)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn get_agent_memory(&self, id: i64) -> Result<Option<MemoryEntry>> {
        Database::get_agent_memory(self, id)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn delete_agent_memory(&self, id: i64) -> Result<bool> {
        Database::delete_agent_memory(self, id).await
    }
    async fn update_agent_memory_in_scopes(
        &self,
        id: i64,
        scopes: &[(String, String)],
        record: AgentMemoryUpdateRecord,
    ) -> Result<Option<MemoryEntry>> {
        Database::update_agent_memory_in_scopes(self, id, scopes, record)
            .await
            .map(|row| row.map(Into::into))
    }
    async fn delete_agent_memory_in_scopes(
        &self,
        id: i64,
        scopes: &[(String, String)],
    ) -> Result<bool> {
        Database::delete_agent_memory_in_scopes(self, id, scopes).await
    }
    async fn recall_memory_candidates(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        Database::recall_memory_candidates(self, scopes, limit).await
    }
    async fn memory_sources(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<nuono_memory::MemorySource>>> {
        Database::companion_memory_sources(self, ids).await
    }
    async fn memory_profiles(
        &self,
        ids: &[i64],
    ) -> Result<Vec<nuono_app::agent::memory::MemoryProfile>> {
        Database::memory_profiles(self, ids).await
    }
    async fn memory_embeddings(
        &self,
        ids: &[i64],
        model: &nuono_ai::embedding::EmbeddingModel,
    ) -> Result<Vec<MemoryEmbeddingRecord>> {
        Database::memory_embeddings(self, ids, model).await
    }
    async fn save_memory_embeddings(&self, records: &[MemoryEmbeddingRecord]) -> Result<usize> {
        Database::save_memory_embeddings(self, records).await
    }
    async fn pin_memory(
        &self,
        id: i64,
        pinned: bool,
    ) -> Result<Option<nuono_app::agent::memory::MemoryProfile>> {
        Database::pin_memory(self, id, pinned).await
    }
    async fn reinforce_memories(
        &self,
        ids: &[i64],
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        Database::reinforce_memories(self, ids, at).await
    }
    async fn memory_scopes(&self) -> Result<Vec<nuono_app::agent::memory::MemoryScope>> {
        Database::memory_scopes(self).await
    }
    async fn memory_graph(
        &self,
        scope: &nuono_app::agent::memory::MemoryScope,
    ) -> Result<Option<nuono_app::agent::memory::MemoryGraphRecord>> {
        Database::memory_graph(self, scope).await
    }
    async fn save_memory_graph(
        &self,
        graph: nuono_app::agent::memory::MemoryGraphRecord,
    ) -> Result<bool> {
        Database::save_memory_graph(self, graph).await
    }
    async fn provider_health(&self, limit: u64) -> Result<Vec<ProviderHealthRecord>> {
        crate::audit::list_provider_health(self, limit).await
    }
    async fn record_event(
        &self,
        event_type: &str,
        bot_id: Option<&str>,
        actor: Option<&str>,
        data: serde_json::Value,
    ) -> Result<()> {
        crate::audit::record_event(self, event_type, bot_id, actor, data).await
    }
}
