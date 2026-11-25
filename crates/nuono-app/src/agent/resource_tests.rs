//! 使用内存模拟存储与模型接口，对 Agent 运行时的并发配额、中断清理及故障恢复进行集成测试。
#[path = "output_stream_tests.rs"]
mod output_stream_tests;
use super::{
    AgentContext, AgentResourceLimits,
    contracts::{AgentTool, AgentToolContext, ToolRiskLevel},
    ports::{AgentConfiguration, ModelProviderFactory},
    runtime::AgentRuntime,
    store::*,
    tool::ToolRegistry,
};
use crate::{ai::*, settings::AIConfig};
use nuono_error::{NuonoError, Result};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Debug, Default)]
struct Store {
    runs: Mutex<HashMap<String, AgentRunRecord>>,
    approvals: Mutex<HashMap<i64, ToolApproval>>,
    fail_create: AtomicBool,
    fail_history: AtomicBool,
    fail_interrupt: AtomicBool,
    interruption_calls: AtomicUsize,
    cleanup_gate: Mutex<Option<Arc<Semaphore>>>,
    create_gate: Mutex<Option<Arc<Semaphore>>>,
    resume_barrier: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    failed_cas_gate: Mutex<Option<Arc<Semaphore>>>,
    failed_cas: AtomicUsize,
    terminal_gate: Mutex<Option<Arc<Semaphore>>>,
    terminal_attempts: AtomicUsize,
    events: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl AgentStore for Store {
    async fn create_agent_run(&self, run: AgentRunRecord) -> Result<()> {
        if self.fail_create.load(Ordering::SeqCst) {
            return Err(NuonoError::DatabaseQuery("injected create failure".into()));
        }
        self.runs.lock().unwrap().insert(run.id.clone(), run);
        let gate = self.create_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn interrupt_agent_run(&self, id: &str, revision: i64, reason: &str) -> Result<bool> {
        self.interruption_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.cleanup_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        if self.fail_interrupt.load(Ordering::SeqCst) {
            return Err(NuonoError::DatabaseQuery(
                "injected interruption failure".into(),
            ));
        }
        let mut runs = self.runs.lock().unwrap();
        let Some(row) = runs.get_mut(id) else {
            return Ok(false);
        };
        if row.revision != revision || row.status != "running" {
            return Ok(false);
        }
        let parent = row.parent_id.clone();
        let mut ids = vec![id.to_owned()];
        if let Some(parent) = parent
            && runs
                .get(&parent)
                .is_some_and(|row| row.status == "waiting_children")
        {
            ids.push(parent);
        }
        let roots = ids.clone();
        ids.extend(
            runs.values()
                .filter(|row| {
                    row.parent_id
                        .as_ref()
                        .is_some_and(|parent| roots.contains(parent))
                        && super::run::is_active(&row.status)
                })
                .map(|row| row.id.clone()),
        );
        for id in ids {
            let row = runs.get_mut(&id).unwrap();
            row.status = "interrupted".into();
            row.revision += 1;
            let mut state: Value = serde_json::from_str(&row.state_json).unwrap();
            state["reply"] = json!(reason);
            state["interruption"] = json!({"reason":reason});
            row.state_json = state.to_string();
        }
        Ok(true)
    }
    async fn update_agent_run(
        &self,
        mut run: AgentRunRecord,
        expected_revision: i64,
    ) -> Result<bool> {
        if run.parent_id.is_none() && matches!(run.status.as_str(), "timed_out" | "budget_exceeded")
        {
            let gate = self.terminal_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                self.terminal_attempts.fetch_add(1, Ordering::SeqCst);
                gate.acquire().await.unwrap().forget();
            }
        }
        let was_waiting = self
            .runs
            .lock()
            .unwrap()
            .get(&run.id)
            .is_some_and(|row| row.status == "waiting_approval");
        let barrier = self.resume_barrier.lock().unwrap().clone();
        if run.status == "running"
            && was_waiting
            && let Some(barrier) = barrier
        {
            barrier.wait().await;
        }
        let changed = {
            let mut runs = self.runs.lock().unwrap();
            if runs
                .get(&run.id)
                .is_none_or(|row| row.revision != expected_revision)
            {
                false
            } else {
                run.revision = expected_revision + 1;
                runs.insert(run.id.clone(), run);
                true
            }
        };
        if !changed {
            self.failed_cas.fetch_add(1, Ordering::SeqCst);
            let gate = self.failed_cas_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
        }
        Ok(changed)
    }
    async fn create_agent_children(
        &self,
        mut parent: AgentRunRecord,
        expected_revision: i64,
        children: Vec<AgentRunRecord>,
    ) -> Result<bool> {
        let mut runs = self.runs.lock().unwrap();
        if runs
            .get(&parent.id)
            .is_none_or(|row| row.revision != expected_revision)
        {
            return Ok(false);
        }
        parent.revision = expected_revision + 1;
        runs.insert(parent.id.clone(), parent);
        for child in children {
            runs.insert(child.id.clone(), child);
        }
        Ok(true)
    }
    async fn claim_agent_child(
        &self,
        mut child: AgentRunRecord,
        expected_revision: i64,
        parent_revision: i64,
    ) -> Result<bool> {
        let mut runs = self.runs.lock().unwrap();
        if runs
            .get(&child.id)
            .is_none_or(|row| row.revision != expected_revision || row.status != "queued")
            || child
                .parent_id
                .as_ref()
                .and_then(|id| runs.get(id))
                .is_none_or(|parent| {
                    parent.revision != parent_revision || parent.status != "running"
                })
        {
            return Ok(false);
        }
        child.revision = expected_revision + 1;
        runs.insert(child.id.clone(), child);
        Ok(true)
    }
    async fn list_agent_children(&self, parent_id: &str) -> Result<Vec<AgentRunRecord>> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|run| run.parent_id.as_deref() == Some(parent_id))
            .cloned()
            .collect())
    }
    async fn list_agent_runs(
        &self,
        _status: Option<&str>,
        _limit: u64,
    ) -> Result<Vec<AgentRunListRecord>> {
        unimplemented!()
    }
    async fn get_agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>> {
        Ok(self.runs.lock().unwrap().get(id).cloned())
    }
    async fn active_agent_run(&self, session_id: &str) -> Result<Option<AgentRunRecord>> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .values()
            .find(|run| run.session_id == session_id && super::run::is_active(&run.status))
            .cloned())
    }
    async fn agent_run_for_approval(&self, id: i64) -> Result<Option<AgentRunRecord>> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .values()
            .find(|run| run.approval_id == Some(id))
            .cloned())
    }
    async fn complete_agent_tool_approval(
        &self,
        _id: i64,
        _status: &str,
        _execution_json: String,
    ) -> Result<Option<ToolApproval>> {
        unimplemented!()
    }
    async fn list_ai_models(&self) -> Result<Vec<ProviderModel>> {
        Ok(Vec::new())
    }
    async fn get_default_ai_model(&self) -> Result<Option<ProviderModel>> {
        Ok(None)
    }
    async fn recent_ai_conversations(
        &self,
        _session_id: &str,
        _limit: u64,
    ) -> Result<Vec<ConversationEntry>> {
        Ok(Vec::new())
    }
    async fn save_ai_conversation(
        &self,
        _session_id: &str,
        _role: &str,
        _content: &str,
    ) -> Result<()> {
        if self.fail_history.load(Ordering::SeqCst) {
            return Err(NuonoError::DatabaseQuery("injected history failure".into()));
        }
        Ok(())
    }
    async fn save_agent_tool_receipt(&self, _receipt: AgentToolReceiptRecord) -> Result<()> {
        Ok(())
    }
    async fn create_agent_tool_approval(&self, input: AgentToolApprovalRecord) -> Result<i64> {
        let mut approvals = self.approvals.lock().unwrap();
        let id = approvals.len() as i64 + 1;
        approvals.insert(
            id,
            ToolApproval {
                id,
                run_id: input.run_id,
                execution_json: None,
                context_json: input.context_json,
                session_id: input.session_id,
                bot_id: input.bot_id,
                platform: input.platform,
                message_type: input.message_type,
                user_id: input.user_id,
                group_id: input.group_id,
                channel_id: input.channel_id,
                message_id: input.message_id,
                tool_name: input.tool_name,
                risk_level: input.risk_level,
                status: "pending".into(),
                input_json: input.input_json,
                reason: input.reason,
                decision_by: None,
                decision_reason: None,
                decided_at: None,
                created_at: chrono::Utc::now(),
            },
        );
        Ok(id)
    }
    async fn decide_agent_tool_approval(
        &self,
        _id: i64,
        _approved: bool,
        _actor: Option<String>,
        _reason: Option<String>,
    ) -> Result<Option<ToolApproval>> {
        unimplemented!()
    }
    async fn claim_agent_tool_approval_for_execution(
        &self,
        _id: i64,
    ) -> Result<Option<ToolApproval>> {
        unimplemented!()
    }
    async fn get_agent_tool_approval(&self, id: i64) -> Result<Option<ToolApproval>> {
        Ok(self.approvals.lock().unwrap().get(&id).cloned())
    }
    async fn mark_agent_tool_approval_status(
        &self,
        _id: i64,
        _status: &str,
    ) -> Result<Option<ToolApproval>> {
        unimplemented!()
    }
    async fn list_agent_memories(
        &self,
        _scope_type: Option<&str>,
        _scope_key: Option<&str>,
        _enabled: Option<bool>,
        _active_only: bool,
        _limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn list_agent_memories_for_scopes(
        &self,
        _scopes: &[(String, String)],
        _limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn upsert_agent_memory(&self, _record: AgentMemoryRecord) -> Result<(MemoryEntry, bool)> {
        unimplemented!()
    }
    async fn update_agent_memory(
        &self,
        _id: i64,
        _record: AgentMemoryUpdateRecord,
    ) -> Result<Option<MemoryEntry>> {
        unimplemented!()
    }
    async fn get_agent_memory(&self, _id: i64) -> Result<Option<MemoryEntry>> {
        Ok(None)
    }
    async fn delete_agent_memory(&self, _id: i64) -> Result<bool> {
        unimplemented!()
    }
    async fn provider_health(&self, _limit: u64) -> Result<Vec<ProviderHealthRecord>> {
        Ok(Vec::new())
    }
    async fn record_event(
        &self,
        event_type: &str,
        _bot_id: Option<&str>,
        _actor: Option<&str>,
        _data: Value,
    ) -> Result<()> {
        self.events.lock().unwrap().push(event_type.into());
        Ok(())
    }
}

struct Config(Mutex<AIConfig>);
impl Default for Config {
    fn default() -> Self {
        Self(Mutex::new(AIConfig {
            enabled: true,
            reply_enabled: true,
            api_key: Some("test".into()),
            agent_autonomy: "approved_tools".into(),
            agent_memory_enabled: false,
            provider_router_enabled: false,
            agent_memory_auto_extract_enabled: false,
            ..Default::default()
        }))
    }
}
#[async_trait::async_trait]
impl AgentConfiguration for Config {
    async fn ai_settings(&self) -> AIConfig {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Slow,
    Approval,
    Delegation,
    ChildApproval,
}

struct Model {
    mode: Mode,
    gate: Semaphore,
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    fail: AtomicBool,
    panic_chat: AtomicBool,
    hold_all: AtomicBool,
}
struct ActiveCall<'a>(&'a AtomicUsize);
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Model {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            gate: Semaphore::new(0),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            panic_chat: AtomicBool::new(false),
            hold_all: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl AiService for Model {
    async fn chat(&self, request: ChatRequest) -> std::result::Result<ChatResponse, AiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let _active = ActiveCall(&self.active);
        assert!(
            !self.panic_chat.load(Ordering::SeqCst),
            "injected provider panic"
        );
        let has_result = request
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Tool);
        let child = request.messages.iter().any(|message| {
            message.role == MessageRole::User && message.content.starts_with("child:")
        });
        if self.hold_all.load(Ordering::SeqCst)
            || matches!(self.mode, Mode::Slow)
            || (matches!(self.mode, Mode::Delegation) && child)
        {
            self.gate.acquire().await.unwrap().forget();
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(AiError::RequestFailed("injected model failure".into()));
        }
        let tool_calls = match self.mode {
            Mode::Approval | Mode::ChildApproval
                if !has_result && (matches!(self.mode, Mode::Approval) || child) =>
            {
                vec![ChatToolCall {
                    id: Some("approve".into()),
                    name: "test.approval".into(),
                    arguments: json!({}),
                }]
            }
            Mode::Delegation | Mode::ChildApproval if !has_result && !child => vec![ChatToolCall {
                id: Some("delegate".into()),
                name: super::children::DELEGATE_TOOL.into(),
                arguments: if matches!(self.mode, Mode::ChildApproval) {
                    json!({"tasks":[{"task":"child:one","tools":["test.approval"]},{"task":"child:two","tools":["test.approval"]}]})
                } else {
                    json!({"tasks":[{"task":"child:one","tools":[]},{"task":"child:two","tools":[]}]})
                },
            }],
            _ => Vec::new(),
        };
        Ok(ChatResponse {
            id: "reply".into(),
            model: "test".into(),
            content: if tool_calls.is_empty() { "done" } else { "" }.into(),
            usage: UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
            finish_reason: None,
            tool_calls,
            continuation: Vec::new(),
        })
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<ChatStreamChunk, AiError>>,
        AiError,
    > {
        unreachable!("Agent uses nonstreaming requests")
    }
    fn service_name(&self) -> &str {
        "test"
    }
}
struct Factory(Arc<Model>);
impl ModelProviderFactory for Factory {
    fn create(&self, _config: &AiConfig) -> std::result::Result<Arc<dyn AiService>, AiError> {
        Ok(self.0.clone())
    }
}

struct ApprovalTool;
#[async_trait::async_trait]
impl AgentTool for ApprovalTool {
    fn name(&self) -> &str {
        "test.approval"
    }
    fn description(&self) -> &str {
        "approval fixture"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::RequiresApproval
    }
    async fn call(&self, _ctx: AgentToolContext, _input: Value) -> Result<Value> {
        panic!("rejected approval must not execute")
    }
}

fn runtime(active: usize, models: usize, mode: Mode) -> (AgentRuntime, Arc<Store>, Arc<Model>) {
    let store = Arc::new(Store::default());
    let model = Arc::new(Model::new(mode));
    let registry = Arc::new(ToolRegistry::new());
    registry.register(ApprovalTool).unwrap();
    let runtime = AgentRuntime::with_ports_and_resource_limits(
        store.clone(),
        Arc::new(Config::default()),
        Arc::new(Factory(model.clone())),
        registry,
        AgentResourceLimits {
            max_active_executions: active,
            max_model_requests: models,
        },
    )
    .unwrap();
    (runtime, store, model)
}

fn context(user: i64, text: &str) -> AgentContext {
    AgentContext::from_runtime_event(
        1,
        &json!({"post_type":"message","message_type":"private","user_id":user,"raw_message":text}),
    )
    .unwrap()
}

async fn until(predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture condition reached");
}

#[tokio::test]
async fn admission_is_shared_fail_fast_and_owned_by_unpolled_future() {
    let (runtime, store, _) = runtime(1, 1, Mode::Slow);
    let first = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap();
    let clone = runtime.clone();
    assert!(
        matches!(clone.submit_text(context(2, "hello")).await, Err(NuonoError::Conflict(message)) if message.contains("名额已满"))
    );
    assert_eq!(
        store.runs.lock().unwrap().len(),
        1,
        "rejection must not create durable work"
    );
    let steer = clone
        .submit_text(context(1, "more"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert!(steer.content.contains("补充消息"));
    let cancel = clone
        .submit_text(context(1, "/cancel"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert!(cancel.content.contains("取消"));
    drop(first);
    runtime.wait_for_idle().await.unwrap();
    let second = clone
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap();
    drop(second);
    runtime.wait_for_idle().await.unwrap();
    store.fail_create.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.submit_text(context(3, "hello")).await,
        Err(NuonoError::DatabaseQuery(_))
    ));
    store.fail_create.store(false, Ordering::SeqCst);
    runtime.wait_for_idle().await.unwrap();
    drop(
        runtime
            .submit_text(context(4, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn slow_sessions_share_model_budget_and_abort_releases_both_slots() {
    let (runtime, _, model) = runtime(4, 2, Mode::Slow);
    let mut handles = Vec::new();
    for user in 1..=4 {
        handles.push(tokio::spawn(
            runtime
                .clone()
                .submit_text(context(user, "hello"))
                .await
                .unwrap()
                .unwrap(),
        ));
        if user == 1 {
            until(|| model.calls.load(Ordering::SeqCst) == 1).await;
        }
    }
    until(|| model.calls.load(Ordering::SeqCst) == 2).await;
    assert_eq!(model.active.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.resources.models.available_permits(), 0);
    // 第一个请求已在提交后续任务前进入了模型调用阶段。
    handles[0].abort();
    let aborted = handles.remove(0).await.unwrap_err();
    assert!(aborted.is_cancelled());
    until(|| model.calls.load(Ordering::SeqCst) == 3).await;
    until(|| runtime.runs.len() == 3).await;
    drop(
        runtime
            .submit_text(context(5, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    model.gate.add_permits(4);
    for handle in handles {
        assert_eq!(handle.await.unwrap().unwrap().content, "done");
    }
    assert_eq!(model.peak.load(Ordering::SeqCst), 2);
    assert_eq!(model.active.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.resources.models.available_permits(), 2);
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn model_error_releases_request_and_execution_budget() {
    let (runtime, _, model) = runtime(1, 1, Mode::Slow);
    model.fail.store(true, Ordering::SeqCst);
    model.gate.add_permits(1);
    let failed = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert!(failed.content.contains("injected model failure"));
    assert_eq!(runtime.resources.models.available_permits(), 1);
    model.fail.store(false, Ordering::SeqCst);
    model.gate.add_permits(1);
    let recovered = runtime
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(recovered.content, "done");
}

#[tokio::test]
async fn approval_pause_releases_admission_and_saturated_resume_preserves_checkpoint() {
    let (runtime, store, model) = runtime(1, 1, Mode::Approval);
    let paused = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let checkpoint = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(checkpoint.status, "waiting_approval");
    let approval = checkpoint.approval_id.unwrap();
    store
        .approvals
        .lock()
        .unwrap()
        .get_mut(&approval)
        .unwrap()
        .status = "rejected".into();
    let blocker = runtime
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(runtime.resume_approval(approval).await, Err(NuonoError::Conflict(message)) if message.contains("名额已满"))
    );
    let unchanged = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(unchanged.status, "waiting_approval");
    assert_eq!(unchanged.revision, checkpoint.revision);
    assert_eq!(unchanged.state_json, checkpoint.state_json);
    drop(blocker);
    runtime.wait_for_idle().await.unwrap();
    let resumed = runtime
        .clone()
        .resume_approval(approval)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.content, "done");
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn children_share_parent_admission_and_model_slots_without_deadlock() {
    let (runtime, store, model) = runtime(1, 1, Mode::Delegation);
    let handle = tokio::spawn(
        runtime
            .submit_text(context(1, "delegate"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 2).await;
    assert_eq!(model.active.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|row| row.parent_id.is_some())
            .count(),
        2
    );
    model.gate.add_permits(2);
    let reply = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.content, "done");
    assert_eq!(model.peak.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|row| row.parent_id.is_some() && row.status == "completed")
            .count(),
        2
    );
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn queued_model_requests_recheck_ai_and_memory_before_sending_context() {
    for disable_memory in [false, true] {
        let store = Arc::new(Store::default());
        let model = Arc::new(Model::new(Mode::Slow));
        let config = Arc::new(Config::default());
        config.0.lock().unwrap().agent_memory_enabled = true;
        let runtime = AgentRuntime::with_ports_and_resource_limits(
            store,
            config.clone(),
            Arc::new(Factory(model.clone())),
            Arc::new(ToolRegistry::new()),
            AgentResourceLimits {
                max_active_executions: 2,
                max_model_requests: 1,
            },
        )
        .unwrap();
        // 先占满模型并发槽位，使后续请求停留在等待准入阶段。
        let permit = runtime
            .resources
            .models
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let queued_runtime = runtime.clone();
        let queued_model = model.clone();
        let mut queued = Box::pin(async {
            let permit = queued_runtime
                .acquire_model_request(super::runtime::ModelRequestPolicy::Conversation {
                    context: &context(1, "saved memory"),
                    uses_memory: true,
                })
                .await?;
            queued_runtime
                .provider_chat(
                    queued_model.as_ref(),
                    ChatRequest::new(vec![ChatMessage::user("saved memory")]),
                    permit,
                )
                .await
        });
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                queued.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        if disable_memory {
            config.0.lock().unwrap().agent_memory_enabled = false;
        } else {
            config.0.lock().unwrap().enabled = false;
        }
        drop(permit);
        let error = queued.await.unwrap_err().to_string();
        assert!(error.contains(if disable_memory {
            "长期记忆已关闭"
        } else {
            "AI 功能已停用"
        }));
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.resources.models.available_permits(), 1);
    }
}

#[tokio::test]
async fn paused_child_approval_reacquires_capacity_then_collects_parent() {
    let (runtime, store, _) = runtime(1, 1, Mode::ChildApproval);
    let paused = runtime
        .submit_text(context(1, "delegate"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        store
            .get_agent_run(&paused.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "waiting_children"
    );
    let children = store.list_agent_children(&paused.run_id).await.unwrap();
    assert_eq!(children.len(), 2);
    assert!(
        children
            .iter()
            .all(|child| child.status == "waiting_approval")
    );
    let blocker = runtime
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap();
    for child in &children {
        let id = child.approval_id.unwrap();
        store.approvals.lock().unwrap().get_mut(&id).unwrap().status = "rejected".into();
        assert!(
            matches!(runtime.resume_approval(id).await, Err(NuonoError::Conflict(message)) if message.contains("名额已满"))
        );
        assert_eq!(
            store
                .get_agent_run(&child.id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            child.revision
        );
    }
    drop(blocker);
    runtime.wait_for_idle().await.unwrap();
    for child in children {
        runtime
            .resume_approval(child.approval_id.unwrap())
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .get_agent_run(&paused.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn saturated_collection_keeps_saved_results_recoverable() {
    let (runtime, store, _) = runtime(1, 1, Mode::ChildApproval);
    let paused = runtime
        .submit_text(context(1, "delegate"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let checkpoint = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(checkpoint.status, "waiting_children");
    // 模拟在父任务收集结果前，其他执行实例已将子任务推进到完成状态。
    for mut child in store.list_agent_children(&paused.run_id).await.unwrap() {
        let mut state = super::run::decode(&child).unwrap();
        state.reply = Some("saved child result".into());
        child.state_json = super::run::encode(&state).unwrap();
        child.status = "completed".into();
        let revision = child.revision;
        assert!(store.update_agent_run(child, revision).await.unwrap());
    }
    let blocker = runtime
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(runtime.collect_children(&paused.run_id).await, Err(NuonoError::Conflict(message)) if message.contains("名额已满"))
    );
    let unchanged = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(unchanged.status, "waiting_children");
    assert_eq!(unchanged.revision, checkpoint.revision);
    drop(blocker);
    runtime.wait_for_idle().await.unwrap();
    assert_eq!(
        runtime
            .collect_children(&paused.run_id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "done"
    );
}

#[tokio::test]
async fn cancelling_a_queued_model_request_releases_admission_without_sending_it() {
    let (runtime, store, model) = runtime(2, 1, Mode::Slow);
    let active = tokio::spawn(
        runtime
            .submit_text(context(1, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 1).await;
    let mut queued = runtime
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap();
    // 轮询一次使其走完前置准备并停在等待模型并发槽位处。
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(queued.as_mut().poll(cx).is_pending()))
            .await
    );
    let id = store
        .runs
        .lock()
        .unwrap()
        .values()
        .find(|row| super::run::decode(row).unwrap().context.user_id == 2)
        .unwrap()
        .id
        .clone();
    runtime.cancel_run(&id).await.unwrap();
    assert!(queued.await.unwrap().content.contains("取消"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.active.load(Ordering::SeqCst), 1);
    drop(
        runtime
            .submit_text(context(3, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    active.abort();
    assert!(active.await.unwrap_err().is_cancelled());
    assert_eq!(runtime.resources.models.available_permits(), 1);
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn model_queue_wait_counts_toward_task_timeout_and_does_not_leak_permits() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    let runtime = runtime.with_run_limits(super::run::RunLimits {
        timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let occupied = runtime
        .resources
        .models
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let reply = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        store
            .get_agent_run(&reply.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "timed_out"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.resources.models.available_permits(), 0);
    drop(occupied);
    assert_eq!(runtime.resources.models.available_permits(), 1);
    model.gate.add_permits(1);
    assert_eq!(
        runtime
            .submit_text(context(2, "hello"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "done"
    );
    runtime.wait_for_idle().await.unwrap();
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn model_admission_wait_does_not_count_as_provider_timeout_or_failure() {
    let store = Arc::new(Store::default());
    let model = Arc::new(Model::new(Mode::Slow));
    let config = Arc::new(Config::default());
    config.0.lock().unwrap().provider_timeout_secs = 1;
    let runtime = AgentRuntime::with_ports_and_resource_limits(
        store.clone(),
        config,
        Arc::new(Factory(model.clone())),
        Arc::new(ToolRegistry::new()),
        AgentResourceLimits {
            max_active_executions: 1,
            max_model_requests: 1,
        },
    )
    .unwrap();
    let occupied = runtime
        .resources
        .models
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let mut task = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(task.as_mut().poll(cx).is_pending()))
            .await
    );
    let handle = tokio::spawn(task);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        !handle.is_finished(),
        "queue wait must not consume provider's one-second request deadline"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !event.starts_with("agent.provider_"))
    );
    model.gate.add_permits(1);
    drop(occupied);
    assert_eq!(handle.await.unwrap().unwrap().content, "done");
    let events = store.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event == "agent.provider_selected")
    );
    assert!(events.iter().all(|event| !matches!(
        event.as_str(),
        "agent.provider_failed" | "agent.provider_timeout"
    )));
}

#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
