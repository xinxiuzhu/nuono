//! Agent 存储层抽象接口与纯数据记录定义（隔离底层 ORM 连接与实体，不向应用层泄露）。
use nuono_error::Result;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentToolReceiptRecord {
    pub context_json: Option<String>,
    pub session_id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: String,
    pub risk_level: String,
    pub status: String,
    pub input_json: Option<String>,
    pub output_json: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentToolApprovalRecord {
    pub run_id: Option<String>,
    pub context_json: Option<String>,
    pub session_id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: String,
    pub risk_level: String,
    pub input_json: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentMemoryRecord {
    pub scope_type: String,
    pub scope_key: String,
    pub memory_key: String,
    pub content: String,
    pub tags_json: Option<String>,
    pub enabled: bool,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub actor: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentMemoryUpdateRecord {
    pub scope_type: Option<String>,
    pub scope_key: Option<String>,
    pub memory_key: Option<String>,
    pub content: Option<String>,
    pub tags_json: Option<String>,
    pub enabled: Option<bool>,
    #[serde(with = "nuono_domain::scalar::nullable_utc_millis")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
    pub actor: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderModel {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub provider_type: String,
    pub model_id: String,
    pub api_key: Option<String>,
    pub base_url: String,
    pub is_default: bool,
    pub enabled: bool,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl std::fmt::Debug for ProviderModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderModel")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("provider", &self.provider)
            .field("provider_type", &self.provider_type)
            .field("model_id", &self.model_id)
            .field("is_default", &self.is_default)
            .field("enabled", &self.enabled)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConversationEntry {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

pub use nuono_memory::MemoryEntry;

/// 记忆条目的向量嵌入缓存记录（可随时按模型重建，始终以原始记忆表为准）。
#[derive(Debug, Clone)]
pub struct MemoryEmbeddingRecord {
    pub memory_id: i64,
    pub model: nuono_ai::embedding::EmbeddingModel,
    pub content_fingerprint: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolApproval {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub execution_json: Option<String>,
    pub context_json: Option<String>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    pub session_id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: String,
    pub risk_level: String,
    pub status: String,
    pub input_json: Option<String>,
    pub reason: Option<String>,
    pub decision_by: Option<String>,
    pub decision_reason: Option<String>,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[serde(default)]
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderHealthRecord {
    pub provider: String,
    pub model: String,
    pub attempts: u64,
    pub successes: u64,
    pub failures: u64,
    pub timeouts: u64,
    pub selected: u64,
    pub success_rate: f64,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub last_elapsed_ms: Option<u64>,
    pub last_seen: Option<String>,
}
#[async_trait::async_trait]
pub trait AgentStore: Send + Sync + std::fmt::Debug {
    /// 解析当前上下文绑定的角色卡（未实现角色卡的简化存储默认返回 `None`，退回全局系统提示词）。
    async fn resolve_ai_persona(
        &self,
        _key: &crate::persona::PersonaBindingKey,
    ) -> Result<Option<crate::persona::PersonaRecord>> {
        Ok(None)
    }
    async fn create_agent_run(&self, run: AgentRunRecord) -> Result<()>;
    async fn update_agent_run(&self, run: AgentRunRecord, expected_revision: i64) -> Result<bool>;
    /// 仅当任务 ID 与版本号（`expected_revision`）匹配且仍处于 `running` 状态时，将其标记为已中断。
    /// 根任务及其所有活跃子任务会在同一事务内一并收敛为中断状态；若中断的是子任务，
    /// 还会连带终止处于 `waiting_children` 的父任务及同批活跃节点。已进入终态或正常等待审批的检查点绝不会被迟到的清理请求覆盖。
    async fn interrupt_agent_run(
        &self,
        _id: &str,
        _expected_revision: i64,
        _reason: &str,
    ) -> Result<bool> {
        Err(nuono_error::NuonoError::Validation(
            "Agent 中断清理存储能力未装配".into(),
        ))
    }
    /// 单实例部署模式下的启动对账操作：在开始接收任何入站事件或审批请求前调用，用于清理上次进程异常退出遗留的活跃任务。
    /// 切勿在构造运行时期间隐式调用，也切勿在多实例共享同一数据库时调用。对账仅收敛任务状态，不会重放工具调用或改动执行中的审批记录。
    async fn reconcile_agent_runs_before_start(
        &self,
        _reason: &str,
    ) -> Result<AgentRunRecoverySummary> {
        Err(nuono_error::NuonoError::Validation(
            "Agent 启动恢复存储能力未装配".into(),
        ))
    }
    /// 在单个数据库事务内原子提交父任务的派发检查点并批量创建所有子任务记录。
    async fn create_agent_children(
        &self,
        parent: AgentRunRecord,
        expected_revision: i64,
        children: Vec<AgentRunRecord>,
    ) -> Result<bool>;
    /// 当父任务版本号与运行状态仍然有效时，以 CAS 方式将排队子任务抢占为 `running` 状态。
    async fn claim_agent_child(
        &self,
        child: AgentRunRecord,
        expected_revision: i64,
        parent_revision: i64,
    ) -> Result<bool>;
    async fn list_agent_children(&self, parent_id: &str) -> Result<Vec<AgentRunRecord>>;
    /// 查询最近更新的根任务列表（不包含子任务；分页条数上限由上层调用方校验）。
    async fn list_agent_runs(
        &self,
        status: Option<&str>,
        limit: u64,
    ) -> Result<Vec<AgentRunListRecord>>;
    async fn get_agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>>;
    async fn active_agent_run(&self, session_id: &str) -> Result<Option<AgentRunRecord>>;
    async fn agent_run_for_approval(&self, id: i64) -> Result<Option<AgentRunRecord>>;
    async fn complete_agent_tool_approval(
        &self,
        id: i64,
        status: &str,
        execution_json: String,
    ) -> Result<Option<ToolApproval>>;
    async fn list_ai_models(&self) -> Result<Vec<ProviderModel>>;
    async fn get_default_ai_model(&self) -> Result<Option<ProviderModel>>;
    async fn recent_ai_conversations(
        &self,
        session_id: &str,
        limit: u64,
    ) -> Result<Vec<ConversationEntry>>;
    async fn save_ai_conversation(&self, session_id: &str, role: &str, content: &str)
    -> Result<()>;
    async fn save_agent_tool_receipt(&self, receipt: AgentToolReceiptRecord) -> Result<()>;
    async fn create_agent_tool_approval(&self, approval: AgentToolApprovalRecord) -> Result<i64>;
    async fn decide_agent_tool_approval(
        &self,
        id: i64,
        approved: bool,
        actor: Option<String>,
        reason: Option<String>,
    ) -> Result<Option<ToolApproval>>;
    async fn claim_agent_tool_approval_for_execution(
        &self,
        id: i64,
    ) -> Result<Option<ToolApproval>>;
    async fn get_agent_tool_approval(&self, id: i64) -> Result<Option<ToolApproval>>;
    async fn mark_agent_tool_approval_status(
        &self,
        id: i64,
        status: &str,
    ) -> Result<Option<ToolApproval>>;
    async fn list_agent_memories(
        &self,
        scope_type: Option<&str>,
        scope_key: Option<&str>,
        enabled: Option<bool>,
        active_only: bool,
        limit: u64,
    ) -> Result<Vec<MemoryEntry>>;
    async fn list_agent_memories_for_scopes(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<MemoryEntry>>;
    async fn upsert_agent_memory(&self, record: AgentMemoryRecord) -> Result<(MemoryEntry, bool)>;
    async fn update_agent_memory(
        &self,
        id: i64,
        record: AgentMemoryUpdateRecord,
    ) -> Result<Option<MemoryEntry>>;
    async fn get_agent_memory(&self, id: i64) -> Result<Option<MemoryEntry>>;
    async fn delete_agent_memory(&self, id: i64) -> Result<bool>;
    /// 仅当目标记忆所属的作用域在允许列表内时，原子地应用字段更新。
    ///
    /// # Errors
    /// 当存储未实现带作用域条件过滤的原子更新或数据库写入失败时返回错误。
    /// 实现方必须在更新语句的查询条件中直接校验作用域，防止并发越权。
    async fn update_agent_memory_in_scopes(
        &self,
        _id: i64,
        _scopes: &[(String, String)],
        _record: AgentMemoryUpdateRecord,
    ) -> Result<Option<MemoryEntry>> {
        Err(nuono_error::NuonoError::NotImplemented(
            "存储未实现原子作用域记忆更新".into(),
        ))
    }
    /// 仅当目标记忆所属的作用域仍在授权列表内时，原子地删除该条记忆。
    ///
    /// # Errors
    /// 当不支持原子作用域的变更或持久化失败时返回错误。
    async fn delete_agent_memory_in_scopes(
        &self,
        _id: i64,
        _scopes: &[(String, String)],
    ) -> Result<bool> {
        Err(nuono_error::NuonoError::NotImplemented(
            "存储未实现原子作用域记忆删除".into(),
        ))
    }
    async fn recall_memory_candidates(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        self.list_agent_memories_for_scopes(scopes, limit).await
    }
    async fn memory_sources(
        &self,
        _ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<nuono_memory::MemorySource>>> {
        Ok(std::collections::HashMap::new())
    }
    async fn memory_profiles(&self, _ids: &[i64]) -> Result<Vec<super::memory::MemoryProfile>> {
        Err(nuono_error::NuonoError::Validation(
            "记忆维护存储能力未装配".into(),
        ))
    }
    async fn memory_embeddings(
        &self,
        _ids: &[i64],
        _model: &nuono_ai::embedding::EmbeddingModel,
    ) -> Result<Vec<MemoryEmbeddingRecord>> {
        Ok(Vec::new())
    }
    async fn save_memory_embeddings(&self, _records: &[MemoryEmbeddingRecord]) -> Result<usize> {
        Err(nuono_error::NuonoError::Validation(
            "记忆向量缓存存储能力未装配".into(),
        ))
    }
    async fn pin_memory(
        &self,
        _id: i64,
        _pinned: bool,
    ) -> Result<Option<super::memory::MemoryProfile>> {
        Err(nuono_error::NuonoError::Validation(
            "记忆维护存储能力未装配".into(),
        ))
    }
    async fn reinforce_memories(
        &self,
        _ids: &[i64],
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        Err(nuono_error::NuonoError::Validation(
            "记忆维护存储能力未装配".into(),
        ))
    }
    async fn memory_scopes(&self) -> Result<Vec<super::memory::MemoryScope>> {
        Err(nuono_error::NuonoError::Validation(
            "记忆维护存储能力未装配".into(),
        ))
    }
    async fn memory_graph(
        &self,
        _scope: &super::memory::MemoryScope,
    ) -> Result<Option<super::memory::MemoryGraphRecord>> {
        Err(nuono_error::NuonoError::Validation(
            "记忆维护存储能力未装配".into(),
        ))
    }
    async fn save_memory_graph(&self, _graph: super::memory::MemoryGraphRecord) -> Result<bool> {
        Err(nuono_error::NuonoError::Validation(
            "记忆维护存储能力未装配".into(),
        ))
    }
    async fn provider_health(&self, limit: u64) -> Result<Vec<ProviderHealthRecord>>;
    async fn record_event(
        &self,
        event_type: &str,
        bot_id: Option<&str>,
        actor: Option<&str>,
        data: serde_json::Value,
    ) -> Result<()>;
}

/// Agent 任务的持久化检查点记录（每次状态更新前均通过 `revision` 进行乐观锁原子比对）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentRunRecord {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub session_id: String,
    pub status: String,
    pub state_json: String,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub approval_id: Option<i64>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
}

/// 启动恢复对账结果统计（仅实际被改写为中断状态的行计入 `interrupted_runs`；
/// 处于 `executing` 状态的审批记录保持原样不自动回滚，因为旧进程停止前可能已产生了外部副作用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentRunRecoverySummary {
    pub interrupted_runs: u64,
    pub interrupted_roots: u64,
    pub unresolved_executing_approvals: u64,
}

/// 根任务列表查询的存储层返回项（检查点 JSON 仍由应用层解码）。
#[derive(Debug, Clone)]
pub struct AgentRunListRecord {
    pub run: AgentRunRecord,
    pub child_count: u64,
    pub updated_at: String,
}
