use nuono_app::{
    accounts::{AccountMutation, AccountService, AccountStore},
    agent::{execution::ToolExecutionService, ports::AgentConfiguration, store::*, tool::*, *},
    auth::{CallerIdentity, Role},
    preferences::{ThemeConfig, ThemeService, ThemeStore},
    settings::AIConfig,
};
use nuono_core::{
    db::{Database, NewUser, STATUS_ACTIVE},
    error::{NuonoError, Result},
    test_support,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
#[derive(Debug, Default)]
struct ReceiptGate {
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}
#[derive(Debug)]
struct ReceiptStore {
    db: Arc<dyn AgentStore>,
    fail_all_receipts: AtomicBool,
    fail_final_receipts: AtomicBool,
    started_receipt_gate: Option<Arc<ReceiptGate>>,
}
#[async_trait::async_trait]
impl AgentStore for ReceiptStore {
    async fn create_agent_children(
        &self,
        parent: AgentRunRecord,
        revision: i64,
        children: Vec<AgentRunRecord>,
    ) -> Result<bool> {
        self.db
            .create_agent_children(parent, revision, children)
            .await
    }
    async fn claim_agent_child(
        &self,
        child: AgentRunRecord,
        expected_revision: i64,
        parent_revision: i64,
    ) -> Result<bool> {
        self.db
            .claim_agent_child(child, expected_revision, parent_revision)
            .await
    }
    async fn list_agent_children(&self, parent_id: &str) -> Result<Vec<AgentRunRecord>> {
        self.db.list_agent_children(parent_id).await
    }
    async fn create_agent_run(&self, run: AgentRunRecord) -> Result<()> {
        self.db.create_agent_run(run).await
    }
    async fn update_agent_run(&self, run: AgentRunRecord, revision: i64) -> Result<bool> {
        self.db.update_agent_run(run, revision).await
    }
    async fn interrupt_agent_run(
        &self,
        id: &str,
        expected_revision: i64,
        reason: &str,
    ) -> Result<bool> {
        self.db
            .interrupt_agent_run(id, expected_revision, reason)
            .await
    }
    async fn reconcile_agent_runs_before_start(
        &self,
        reason: &str,
    ) -> Result<AgentRunRecoverySummary> {
        self.db.reconcile_agent_runs_before_start(reason).await
    }
    async fn get_agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>> {
        self.db.get_agent_run(id).await
    }
    async fn list_agent_runs(
        &self,
        status: Option<&str>,
        limit: u64,
    ) -> Result<Vec<AgentRunListRecord>> {
        self.db.list_agent_runs(status, limit).await
    }
    async fn active_agent_run(&self, session: &str) -> Result<Option<AgentRunRecord>> {
        self.db.active_agent_run(session).await
    }
    async fn agent_run_for_approval(&self, id: i64) -> Result<Option<AgentRunRecord>> {
        self.db.agent_run_for_approval(id).await
    }
    async fn complete_agent_tool_approval(
        &self,
        id: i64,
        status: &str,
        execution: String,
    ) -> Result<Option<ToolApproval>> {
        self.db
            .complete_agent_tool_approval(id, status, execution)
            .await
    }
    async fn list_ai_models(&self) -> Result<Vec<ProviderModel>> {
        self.db.list_ai_models().await
    }
    async fn get_default_ai_model(&self) -> Result<Option<ProviderModel>> {
        self.db.get_default_ai_model().await
    }
    async fn recent_ai_conversations(
        &self,
        session_id: &str,
        limit: u64,
    ) -> Result<Vec<ConversationEntry>> {
        self.db.recent_ai_conversations(session_id, limit).await
    }
    async fn save_ai_conversation(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        self.db
            .save_ai_conversation(session_id, role, content)
            .await
    }
    async fn save_agent_tool_receipt(&self, receipt: AgentToolReceiptRecord) -> Result<()> {
        if self.fail_all_receipts.load(Ordering::SeqCst)
            || (self.fail_final_receipts.load(Ordering::SeqCst) && receipt.status != "Started")
        {
            return Err(NuonoError::DatabaseQuery("injected receipt failure".into()));
        }
        if receipt.status == "Started"
            && let Some(gate) = &self.started_receipt_gate
        {
            gate.entered.notify_one();
            gate.released.notified().await;
        }
        self.db.save_agent_tool_receipt(receipt).await
    }
    async fn create_agent_tool_approval(&self, approval: AgentToolApprovalRecord) -> Result<i64> {
        self.db.create_agent_tool_approval(approval).await
    }
    async fn decide_agent_tool_approval(
        &self,
        id: i64,
        approved: bool,
        actor: Option<String>,
        reason: Option<String>,
    ) -> Result<Option<ToolApproval>> {
        self.db
            .decide_agent_tool_approval(id, approved, actor, reason)
            .await
    }
    async fn claim_agent_tool_approval_for_execution(
        &self,
        id: i64,
    ) -> Result<Option<ToolApproval>> {
        self.db.claim_agent_tool_approval_for_execution(id).await
    }
    async fn get_agent_tool_approval(&self, id: i64) -> Result<Option<ToolApproval>> {
        self.db.get_agent_tool_approval(id).await
    }
    async fn mark_agent_tool_approval_status(
        &self,
        id: i64,
        status: &str,
    ) -> Result<Option<ToolApproval>> {
        self.db.mark_agent_tool_approval_status(id, status).await
    }
    async fn list_agent_memories(
        &self,
        scope_type: Option<&str>,
        scope_key: Option<&str>,
        enabled: Option<bool>,
        active_only: bool,
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        self.db
            .list_agent_memories(scope_type, scope_key, enabled, active_only, limit)
            .await
    }
    async fn list_agent_memories_for_scopes(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        self.db.list_agent_memories_for_scopes(scopes, limit).await
    }
    async fn upsert_agent_memory(&self, record: AgentMemoryRecord) -> Result<(MemoryEntry, bool)> {
        self.db.upsert_agent_memory(record).await
    }
    async fn update_agent_memory(
        &self,
        id: i64,
        record: AgentMemoryUpdateRecord,
    ) -> Result<Option<MemoryEntry>> {
        self.db.update_agent_memory(id, record).await
    }
    async fn get_agent_memory(&self, id: i64) -> Result<Option<MemoryEntry>> {
        self.db.get_agent_memory(id).await
    }
    async fn delete_agent_memory(&self, id: i64) -> Result<bool> {
        self.db.delete_agent_memory(id).await
    }
    async fn provider_health(&self, limit: u64) -> Result<Vec<ProviderHealthRecord>> {
        self.db.provider_health(limit).await
    }
    async fn record_event(
        &self,
        event_type: &str,
        bot_id: Option<&str>,
        actor: Option<&str>,
        data: serde_json::Value,
    ) -> Result<()> {
        self.db.record_event(event_type, bot_id, actor, data).await
    }
}
struct Config(tokio::sync::RwLock<AIConfig>);
#[async_trait::async_trait]
impl AgentConfiguration for Config {
    async fn ai_settings(&self) -> AIConfig {
        self.0.read().await.clone()
    }
}
struct Counter {
    calls: Arc<AtomicUsize>,
    risk: ToolRiskLevel,
}
#[async_trait::async_trait]
impl AgentTool for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    fn description(&self) -> &str {
        "Count effects"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        self.risk
    }
    async fn call(&self, _ctx: AgentToolContext, _input: Value) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"done":true}))
    }
}
fn context() -> AgentContext {
    AgentContext {
        account_chat: None,
        images: Vec::new(),
        reply_to_message_id: None,
        persona: None,
        native: None,
        bot_id: 1,
        platform: "onebot".into(),
        message_type: "private".into(),
        user_id: 2,
        group_id: None,
        channel_id: None,
        message_id: None,
        raw_message: "test".into(),
        content: "test".into(),
    }
}
fn config() -> Arc<Config> {
    let ai = AIConfig {
        enabled: true,
        reply_enabled: true,
        agent_autonomy: "approved_tools".into(),
        ..AIConfig::default()
    };
    Arc::new(Config(tokio::sync::RwLock::new(ai)))
}
fn tools(risk: ToolRiskLevel) -> (Arc<ToolRegistry>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::new());
    tools
        .register(Counter {
            calls: calls.clone(),
            risk,
        })
        .unwrap();
    (tools, calls)
}

#[tokio::test]
async fn invalid_structured_input_is_rejected_before_creating_approval() {
    struct WriteTool;
    #[async_trait::async_trait]
    impl AgentTool for WriteTool {
        fn name(&self) -> &str {
            "structured.write"
        }
        fn description(&self) -> &str {
            "Write a chosen item"
        }
        fn risk_level(&self) -> ToolRiskLevel {
            ToolRiskLevel::RequiresApproval
        }
        fn input_schema(&self) -> Option<Value> {
            Some(
                json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}}),
            )
        }
        async fn call(&self, _: AgentToolContext, _: Value) -> Result<Value> {
            panic!("unapproved call")
        }
    }
    let db = test_support::test_db(2).await;
    let registry = Arc::new(ToolRegistry::new());
    registry.register(WriteTool).unwrap();
    let service = ToolExecutionService::new(Arc::new(db), config(), registry);
    let invalid = service
        .execute(
            context(),
            AutonomyLevel::ApprovedTools,
            "structured.write",
            json!({"id":0}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(invalid.status, ToolExecutionStatus::Rejected);
    assert!(invalid.output.get("approval_id").is_none());
    let valid = service
        .execute(
            context(),
            AutonomyLevel::ApprovedTools,
            "structured.write",
            json!({"id":1}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(valid.status, ToolExecutionStatus::PendingApproval);
}
#[tokio::test]
async fn concurrent_approvals_execute_once_and_record_authenticated_actor() {
    let db = test_support::test_db(5).await;
    let (tools, calls) = tools(ToolRiskLevel::RequiresApproval);
    let service = ToolExecutionService::new(Arc::new(db.clone()), config(), tools);
    let pending = service
        .execute(
            context(),
            AutonomyLevel::ApprovedTools,
            "counter",
            json!({}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(pending.status, ToolExecutionStatus::PendingApproval);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let id = nuono_domain::scalar::wire_i64(&pending.output["approval_id"]).unwrap();
    let user = CallerIdentity::new(3, "user", Role::User);
    assert!(service.decide(&user, id, true, None).await.is_err());
    let a = CallerIdentity::new(1, "owner-a", Role::Owner);
    let b = CallerIdentity::new(2, "owner-b", Role::Owner);
    let (ra, rb) = tokio::join!(
        service.decide(&a, id, true, None),
        service.decide(&b, id, true, None)
    );
    assert!(ra.is_ok());
    assert!(rb.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let row = db.get_agent_tool_approval(id).await.unwrap().unwrap();
    assert_eq!(row.status, "executed");
    assert!(matches!(
        row.decision_by.as_deref(),
        Some("owner-a" | "owner-b")
    ));
    assert_eq!(
        serde_json::from_str::<AgentContext>(row.context_json.as_deref().unwrap())
            .unwrap()
            .user_identity(),
        "2"
    );
}
#[tokio::test]
async fn approval_rechecks_current_policy_before_effects() {
    let db = test_support::test_db(3).await;
    let (tools, calls) = tools(ToolRiskLevel::RequiresApproval);
    let cfg = config();
    let service = ToolExecutionService::new(Arc::new(db.clone()), cfg.clone(), tools);
    let pending = service
        .execute(
            context(),
            AutonomyLevel::ApprovedTools,
            "counter",
            json!({}),
            "test",
        )
        .await
        .unwrap();
    let id = nuono_domain::scalar::wire_i64(&pending.output["approval_id"]).unwrap();
    cfg.0.write().await.agent_denied_tools = vec!["counter".into()];
    let row = service
        .decide(
            &CallerIdentity::new(1, "owner", Role::Owner),
            id,
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(row.status, "execute_failed");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn intent_failure_blocks_effects_and_outcome_failure_does_not_retry() {
    let db = test_support::test_db(3).await;
    let store = Arc::new(ReceiptStore {
        db: Arc::new(db),
        fail_all_receipts: AtomicBool::new(true),
        fail_final_receipts: AtomicBool::new(false),
        started_receipt_gate: None,
    });
    let (tools, calls) = tools(ToolRiskLevel::LowRisk);
    let service = ToolExecutionService::new(store.clone(), config(), tools);
    assert!(
        service
            .execute(
                context(),
                AutonomyLevel::LowRiskTools,
                "counter",
                json!({}),
                "test"
            )
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    store.fail_all_receipts.store(false, Ordering::SeqCst);
    store.fail_final_receipts.store(true, Ordering::SeqCst);
    let result = service
        .execute(
            context(),
            AutonomyLevel::LowRiskTools,
            "counter",
            json!({}),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolExecutionStatus::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn memory_disabled_during_intent_persistence_blocks_all_memory_effects() {
    let db = test_support::test_db(3).await;
    let (saved, _) = db
        .upsert_agent_memory(AgentMemoryRecord {
            scope_type: "global".into(),
            scope_key: "global".into(),
            memory_key: "existing".into(),
            content: "original memory".into(),
            tags_json: None,
            enabled: true,
            expires_at: None,
            actor: None,
        })
        .await
        .unwrap();
    let gate = Arc::new(ReceiptGate::default());
    let store = Arc::new(ReceiptStore {
        db: Arc::new(db.clone()),
        fail_all_receipts: AtomicBool::new(false),
        fail_final_receipts: AtomicBool::new(false),
        started_receipt_gate: Some(gate.clone()),
    });
    let cfg = config();
    cfg.0.write().await.agent_autonomy = "admin_approved".into();
    let service = Arc::new(ToolExecutionService::new(
        store,
        cfg.clone(),
        BuiltinToolRegistry::read_only(),
    ));
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
        cfg.0.write().await.agent_memory_enabled = true;
        let execution = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .execute(context(), AutonomyLevel::AdminApproved, name, input, "test")
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("tool did not reach execution-intent persistence");
        cfg.0.write().await.agent_memory_enabled = false;
        gate.released.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
            .await
            .expect("tool did not finish after releasing intent persistence")
            .unwrap()
            .unwrap();
        assert_eq!(result.status, ToolExecutionStatus::Rejected, "{name}");
        assert!(
            result.output["error"]
                .as_str()
                .unwrap()
                .contains("长期记忆已关闭"),
            "{name}"
        );
        assert_eq!(
            db.get_agent_memory(saved.id)
                .await
                .unwrap()
                .unwrap()
                .content,
            "original memory",
            "{name}"
        );
        assert_eq!(
            db.list_agent_memories(None, None, None, false, 20)
                .await
                .unwrap()
                .len(),
            1,
            "{name}"
        );
    }
}
#[tokio::test]
async fn in_flight_model_request_cannot_retain_revoked_autonomy() {
    let db = test_support::test_db(3).await;
    let (tools, calls) = tools(ToolRiskLevel::LowRisk);
    let cfg = config();
    cfg.0.write().await.agent_autonomy = "reply_only".into();
    let service = ToolExecutionService::new(Arc::new(db), cfg, tools);
    let result = service
        .execute(
            context(),
            AutonomyLevel::AdminApproved,
            "counter",
            json!({}),
            "old request",
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolExecutionStatus::Rejected);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
async fn owner(db: &Database, name: &str) -> i64 {
    db.create_user(NewUser {
        username: name.into(),
        email: None,
        password: "correct-horse-battery".into(),
        role: "owner".into(),
        status: STATUS_ACTIVE.into(),
        email_verified: true,
    })
    .await
    .unwrap()
    .id
}
#[tokio::test]
async fn simultaneous_owner_demotions_leave_one_active_owner() {
    let db = test_support::test_db(5).await;
    let a = owner(&db, "owner-a").await;
    let b = owner(&db, "owner-b").await;
    let (ra, rb) = tokio::join!(
        db.mutate_account(a, b, AccountMutation::Role(Role::User)),
        db.mutate_account(b, a, AccountMutation::Role(Role::User))
    );
    assert_ne!(ra.is_ok(), rb.is_ok());
    assert_eq!(db.owner_count().await.unwrap(), 1);
}
#[tokio::test]
async fn stale_owner_identity_is_rechecked_inside_account_transaction() {
    let db = test_support::test_db(3).await;
    let a = owner(&db, "owner-a").await;
    let b = owner(&db, "owner-b").await;
    let stale = CallerIdentity::new(b, "owner-b", Role::Owner);
    AccountService::new(&db)
        .set_role(
            &CallerIdentity::new(a, "owner-a", Role::Owner),
            b,
            Role::User,
        )
        .await
        .unwrap();
    assert!(
        AccountService::new(&db)
            .set_status(&stale, a, "disabled")
            .await
            .is_err()
    );
    assert_eq!(
        db.find_user_by_id(a).await.unwrap().unwrap().status,
        "active"
    );
}
#[tokio::test]
async fn theme_initialization_never_overwrites_saved_preferences() {
    let db = test_support::test_db(3).await;
    let service = ThemeService::new(&db);
    let mut seed = ThemeConfig {
        primary_color: "blue".into(),
        ..ThemeConfig::default()
    };
    service.initialize(&seed).await.unwrap();
    seed.primary_color = "red".into();
    service.save(&seed).await.unwrap();
    seed.primary_color = "green".into();
    service.initialize(&seed).await.unwrap();
    assert_eq!(db.load_theme().await.unwrap().unwrap().primary_color, "red");
    seed.wallpaper_opacity = 2.0;
    assert!(service.save(&seed).await.is_err());
    assert_eq!(service.get().await.unwrap().primary_color, "red");
}
#[tokio::test]
async fn extended_session_keys_preserve_existing_history() {
    let db = test_support::test_db(3).await;
    db.save_ai_conversation("old", "user", "old history")
        .await
        .unwrap();
    let long = format!("v2:{}", "a".repeat(64));
    db.save_ai_conversation(&long, "user", "new history")
        .await
        .unwrap();
    assert_eq!(
        db.recent_ai_conversations("old", 20).await.unwrap()[0].content,
        "old history"
    );
    assert_eq!(
        db.recent_ai_conversations(&long, 20).await.unwrap()[0].content,
        "new history"
    );
}
