//! 针对真实 PostgreSQL / SQLite 存储验证可选长期记忆开关的行为约定。
use async_trait::async_trait;
use nuono_app::{
    agent::{
        AgentContext, AutonomyLevel,
        execution::ToolExecutionService,
        ports::{AgentConfiguration, ModelProviderFactory},
        runtime::AgentRuntime,
        store::{AgentMemoryRecord, AgentMemoryUpdateRecord},
        tool::{BuiltinToolRegistry, ToolExecutionStatus, ToolRegistry},
    },
    ai::{
        AiConfig, AiError, AiService, ChatRequest, ChatResponse, ChatStreamChunk, MessageRole,
        UsageInfo,
    },
    auth::{CallerIdentity, Role},
    settings::AIConfig,
};
use nuono_core::{db::Database, test_support};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

struct Config(RwLock<AIConfig>);
#[async_trait]
impl AgentConfiguration for Config {
    async fn ai_settings(&self) -> AIConfig {
        self.0.read().await.clone()
    }
}

#[derive(Default)]
struct Model(Mutex<Vec<ChatRequest>>, Mutex<Option<Arc<Config>>>);
#[async_trait]
impl AiService for Model {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AiError> {
        self.0.lock().unwrap().push(request);
        let disable_memory = self.1.lock().unwrap().take();
        if let Some(config) = disable_memory {
            config.0.write().await.agent_memory_enabled = false;
            return Err(AiError::RequestFailed("fixture primary failure".into()));
        }
        Ok(ChatResponse {
            id: "fixture-response".into(),
            model: "fixture-model".into(),
            content: "收到".into(),
            usage: UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
            finish_reason: Some("stop".into()),
            tool_calls: Vec::new(),
            continuation: Vec::new(),
        })
    }

    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChatStreamChunk, AiError>>, AiError> {
        Err(AiError::Other("fixture uses complete messages".into()))
    }

    fn service_name(&self) -> &str {
        "memory-test"
    }
}
struct Factory(Arc<Model>);
impl ModelProviderFactory for Factory {
    fn create(&self, _: &AiConfig) -> Result<Arc<dyn AiService>, AiError> {
        Ok(self.0.clone())
    }
}

struct Harness {
    db: Database,
    config: Arc<Config>,
    model: Arc<Model>,
    tools: Arc<ToolRegistry>,
}
impl Harness {
    async fn new() -> Self {
        Self {
            db: test_support::test_db(3).await,
            config: Arc::new(Config(RwLock::new(AIConfig {
                enabled: true,
                reply_enabled: true,
                provider: "fixture".into(),
                model: "fixture-model".into(),
                api_key: Some("fixture".into()),
                provider_router_enabled: false,
                provider_health_fuse_enabled: false,
                agent_autonomy: "admin_approved".into(),
                agent_memory_enabled: false,
                ..AIConfig::default()
            }))),
            model: Arc::new(Model::default()),
            tools: BuiltinToolRegistry::read_only(),
        }
    }
    async fn chat(&self, text: &str) -> ChatRequest {
        let runtime = AgentRuntime::with_ports(
            Arc::new(self.db.clone()),
            self.config.clone(),
            Arc::new(Factory(self.model.clone())),
            self.tools.clone(),
        );
        let reply = runtime
            .submit_text(context(text))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap();
        assert_eq!(reply.content, "收到");
        self.model.0.lock().unwrap().last().unwrap().clone()
    }
    fn service(&self) -> ToolExecutionService {
        ToolExecutionService::new(
            Arc::new(self.db.clone()),
            self.config.clone(),
            self.tools.clone(),
        )
    }
}

fn context(text: &str) -> AgentContext {
    AgentContext {
        account_chat: None,
        persona: None,
        native: None,
        bot_id: 1,
        platform: "onebot".into(),
        message_type: "private".into(),
        user_id: 2,
        group_id: None,
        channel_id: None,
        message_id: None,
        raw_message: text.into(),
        content: text.into(),
        images: Vec::new(),
        reply_to_message_id: None,
    }
}
fn memory(key: &str, content: &str) -> AgentMemoryRecord {
    AgentMemoryRecord {
        scope_type: "global".into(),
        scope_key: "global".into(),
        memory_key: key.into(),
        content: content.into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        actor: None,
    }
}
fn has_system_memory(request: &ChatRequest) -> bool {
    request.messages.iter().any(|message| {
        message.role == MessageRole::System && message.content.contains("stored-memory-sentinel")
    })
}
fn has_memory_tool(request: &ChatRequest) -> bool {
    request
        .tools
        .iter()
        .any(|tool| tool.name.starts_with("memory."))
}

#[tokio::test]
async fn memory_is_optional_and_auto_capture_is_subordinate_to_the_total_switch() {
    let h = Harness::new().await;
    assert!(!h.config.0.read().await.agent_memory_enabled);
    assert!(h.config.0.read().await.agent_memory_auto_extract_enabled);
    h.db.upsert_agent_memory(memory("existing", "stored-memory-sentinel"))
        .await
        .unwrap();

    let disabled = h.chat("记住：关闭时不应记录这句话").await;
    assert!(!has_system_memory(&disabled));
    assert!(!has_memory_tool(&disabled));
    assert_eq!(
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap()
            .len(),
        1
    );

    h.config.0.write().await.agent_memory_enabled = true;
    let enabled = h.chat("remember: stored-memory-sentinel").await;
    assert!(has_system_memory(&enabled));
    assert!(has_memory_tool(&enabled));
    let rows =
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .filter(|row| row.content == "stored-memory-sentinel")
            .count(),
        2
    );
    assert!(!rows.iter().any(|row| row.content == "关闭时不应记录这句话"));

    h.config.0.write().await.agent_memory_auto_extract_enabled = false;
    let read_only_preference = h.chat("remember: stored-memory-sentinel").await;
    assert!(has_system_memory(&read_only_preference));
    assert!(has_memory_tool(&read_only_preference));
    assert_eq!(
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap()
            .len(),
        2
    );

    h.config.0.write().await.agent_memory_enabled = false;
    let disabled_again = h.chat("正常聊天继续").await;
    assert!(!has_system_memory(&disabled_again));
    assert!(!has_memory_tool(&disabled_again));
    // 短期会话上下文与长期记忆彼此独立，关闭长期记忆不影响短期对话历史。
    assert!(
        disabled_again
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Assistant && message.content == "收到")
    );
    assert_eq!(
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn disabled_memory_blocks_direct_tools_but_preserves_management_crud() {
    let h = Harness::new().await;
    let (saved, _) =
        h.db.upsert_agent_memory(memory("managed", "stored-memory-sentinel"))
            .await
            .unwrap();
    for (name, input) in [
        ("memory.read", json!({})),
        (
            "memory.write",
            json!({"memory_key":"attempt", "content":"must not write"}),
        ),
        (
            "memory.update",
            json!({"id":saved.id, "content":"must not update"}),
        ),
        ("memory.delete", json!({"id":saved.id})),
    ] {
        let result = h
            .service()
            .execute(
                context("工具请求"),
                AutonomyLevel::AdminApproved,
                name,
                input,
                "test",
            )
            .await
            .unwrap();
        assert_eq!(result.status, ToolExecutionStatus::Rejected, "{name}");
        assert!(
            result.output["error"]
                .as_str()
                .unwrap()
                .contains("长期记忆已关闭")
        );
    }
    assert_eq!(
        h.db.get_agent_memory(saved.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "stored-memory-sentinel"
    );
    assert_eq!(
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap()
            .len(),
        1
    );

    h.config.0.write().await.agent_memory_enabled = true;
    let read = h
        .service()
        .execute(
            context("读取 stored-memory-sentinel"),
            AutonomyLevel::AdminApproved,
            "memory.read",
            json!({}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(read.status, ToolExecutionStatus::Succeeded);
    assert_eq!(
        read.output["memories"][0]["content"],
        "stored-memory-sentinel"
    );
    let write = h
        .service()
        .execute(
            context("保存"),
            AutonomyLevel::AdminApproved,
            "memory.write",
            json!({"memory_key":"tool-written", "content":"tool content"}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(write.status, ToolExecutionStatus::Succeeded);
    assert_eq!(
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap()
            .len(),
        2
    );

    h.config.0.write().await.agent_memory_enabled = false;
    // 管理端读写接口共用底层存储，关闭 Agent 长期记忆开关后管理接口依然可用。
    let updated =
        h.db.update_agent_memory(
            saved.id,
            AgentMemoryUpdateRecord {
                content: Some("updated by management".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.content, "updated by management");
    assert!(h.db.delete_agent_memory(saved.id).await.unwrap());
    let (replacement, _) =
        h.db.upsert_agent_memory(memory("new-managed", "saved while disabled"))
            .await
            .unwrap();
    assert_eq!(
        h.db.get_agent_memory(replacement.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "saved while disabled"
    );
}

#[tokio::test]
async fn approval_cannot_bypass_memory_disabled_after_the_request() {
    let h = Harness::new().await;
    {
        let mut cfg = h.config.0.write().await;
        cfg.agent_memory_enabled = true;
        cfg.agent_autonomy = "approved_tools".into();
    }
    let service = h.service();
    let pending = service
        .execute(
            context("保存"),
            AutonomyLevel::ApprovedTools,
            "memory.write",
            json!({"memory_key":"pending", "content":"must not be stored"}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(pending.status, ToolExecutionStatus::PendingApproval);
    let id = nuono_domain::scalar::wire_i64(&pending.output["approval_id"]).unwrap();
    h.config.0.write().await.agent_memory_enabled = false;
    let approval = service
        .decide(
            &CallerIdentity::new(1, "owner", Role::Owner),
            id,
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(approval.status, "execute_failed");
    let execution: serde_json::Value =
        serde_json::from_str(approval.execution_json.as_deref().unwrap()).unwrap();
    assert_eq!(execution["status"], "Rejected");
    assert!(
        h.db.list_agent_memories(None, None, None, false, 20)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn disabling_memory_after_primary_failure_prevents_forwarding_to_a_backup() {
    let h = Harness::new().await;
    let primary =
        h.db.create_ai_model(nuono_core::db::CreateAiModel {
            name: "Primary",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "primary-model",
            api_key: Some("fixture"),
            base_url: "https://example.invalid/v1",
            is_default: true,
        })
        .await
        .unwrap();
    let backup =
        h.db.create_ai_model(nuono_core::db::CreateAiModel {
            name: "Backup",
            provider: "fixture",
            provider_type: "responses",
            model_id: "backup-model",
            api_key: Some("fixture"),
            base_url: "https://example.invalid/v1",
            is_default: false,
        })
        .await
        .unwrap();
    {
        let mut cfg = h.config.0.write().await;
        cfg.agent_memory_enabled = true;
        cfg.provider_router_enabled = true;
        cfg.chat_model_alias = format!("id:{}", primary.id);
        cfg.provider_fallback_chain = vec![format!("id:{}", backup.id)];
    }
    h.db.upsert_agent_memory(memory("existing", "stored-memory-sentinel"))
        .await
        .unwrap();
    *h.model.1.lock().unwrap() = Some(h.config.clone());
    let runtime = AgentRuntime::with_ports(
        Arc::new(h.db.clone()),
        h.config.clone(),
        Arc::new(Factory(h.model.clone())),
        h.tools.clone(),
    );
    let reply = runtime
        .submit_text(context("stored-memory-sentinel"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert!(reply.content.contains("长期记忆已关闭"));
    let requests = h.model.0.lock().unwrap();
    assert_eq!(requests.len(), 1, "disabled memory reached a backup model");
    assert_eq!(requests[0].model.as_deref(), Some("primary-model"));
    assert!(has_system_memory(&requests[0]));
}
