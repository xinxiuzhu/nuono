//! 针对真实 PostgreSQL / SQLite 持久化层验证 Agent 执行循环的核心行为约定。
use async_trait::async_trait;
use nuono_app::{
    agent::{
        AgentContext,
        children::SubAgentLimits,
        execution::ToolExecutionService,
        ports::{AgentConfiguration, ModelProviderFactory},
        run::RunLimits,
        runtime::AgentRuntime,
        tool::{
            AgentTool, AgentToolContext, ToolExecutionStatus, ToolRegistry, ToolResource,
            ToolRiskLevel, ToolScheduling,
        },
    },
    ai::{
        AiConfig, AiError, AiService, ChatRequest, ChatResponse, ChatStreamChunk, ChatToolCall,
        MessageRole, UsageInfo,
    },
    auth::{CallerIdentity, Role},
    settings::AIConfig,
};
use nuono_core::{db::Database, error::Result, test_support};
use sea_orm::{ConnectionTrait, EntityTrait};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    future::{Future, poll_fn},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::{Notify, RwLock};

#[derive(Default)]
struct Gate {
    entered: Notify,
    released: Notify,
}
struct Step {
    response: ChatResponse,
    gate: Option<Arc<Gate>>,
    panic_after_start: bool,
}
struct Script {
    streaming: std::sync::atomic::AtomicBool,
    steps: Mutex<VecDeque<Step>>,
    routes: Mutex<HashMap<String, VecDeque<Step>>>,
    requests: Mutex<Vec<ChatRequest>>,
}
#[async_trait]
impl AiService for Script {
    async fn chat(&self, request: ChatRequest) -> std::result::Result<ChatResponse, AiError> {
        let task = request
            .messages
            .iter()
            .find(|message| message.role == MessageRole::User)
            .map(|message| message.content.clone())
            .unwrap_or_default();
        self.requests.lock().unwrap().push(request);
        let routed = self
            .routes
            .lock()
            .unwrap()
            .get_mut(&task)
            .map(|steps| steps.pop_front().expect("unexpected extra child inference"));
        let step = routed.unwrap_or_else(|| {
            self.steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra inference")
        });
        if let Some(gate) = step.gate {
            gate.entered.notify_one();
            gate.released.notified().await;
        }
        assert!(!step.panic_after_start, "injected model panic");
        Ok(step.response)
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<ChatStreamChunk, AiError>>,
        AiError,
    > {
        if self.streaming.load(Ordering::SeqCst) {
            let response = self.chat(request).await?;
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.try_send(Ok(ChatStreamChunk {
                id: response.id,
                model: response.model,
                delta_content: response.content,
                finish_reason: Some("stop".into()),
            }))
            .unwrap();
            return Ok(rx);
        }
        Err(AiError::Other(
            "test provider uses complete tool messages".into(),
        ))
    }
    fn service_name(&self) -> &str {
        "script"
    }
}
struct Factory(Arc<Script>);
impl ModelProviderFactory for Factory {
    fn create(&self, _: &AiConfig) -> std::result::Result<Arc<dyn AiService>, AiError> {
        Ok(self.0.clone())
    }
}
struct Config(RwLock<AIConfig>);
#[async_trait]
impl AgentConfiguration for Config {
    async fn ai_settings(&self) -> AIConfig {
        self.0.read().await.clone()
    }
}
struct Counter {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<AgentContext>>>,
    risk: ToolRiskLevel,
}
#[async_trait]
impl AgentTool for Counter {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Test tool with observable invocation count"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        self.risk
    }
    async fn call(&self, ctx: AgentToolContext, input: Value) -> Result<Value> {
        self.contexts.lock().unwrap().push(ctx.agent);
        let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(json!({"count": count, "input": input}))
    }
}
struct Harness {
    db: Database,
    config: Arc<Config>,
    script: Arc<Script>,
    registry: Arc<ToolRegistry>,
    reads: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<AgentContext>>>,
}
impl Harness {
    async fn new(steps: Vec<Step>) -> Self {
        let db = test_support::test_db(3).await;
        db.ensure_ai_module().await.unwrap();
        let config = Arc::new(Config(RwLock::new(AIConfig {
            enabled: true,
            reply_enabled: true,
            provider: "script".into(),
            model: "test-model".into(),
            api_key: Some("fixture".into()),
            provider_router_enabled: false,
            provider_health_fuse_enabled: false,
            agent_memory_auto_extract_enabled: false,
            agent_autonomy: "approved_tools".into(),
            max_tokens: 1024,
            ..AIConfig::default()
        })));
        let script = Arc::new(Script {
            streaming: std::sync::atomic::AtomicBool::new(false),
            steps: Mutex::new(steps.into()),
            routes: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
        });
        let registry = Arc::new(ToolRegistry::new());
        let reads = Arc::new(AtomicUsize::new(0));
        let effects = Arc::new(AtomicUsize::new(0));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        for (name, calls, risk) in [
            ("lookup", reads.clone(), ToolRiskLevel::ReadOnly),
            ("change", effects.clone(), ToolRiskLevel::RequiresApproval),
        ] {
            registry
                .register(Counter {
                    name,
                    calls,
                    contexts: contexts.clone(),
                    risk,
                })
                .unwrap();
        }
        Self {
            db,
            config,
            script,
            registry,
            reads,
            effects,
            contexts,
        }
    }
    fn runtime(&self) -> AgentRuntime {
        AgentRuntime::with_ports(
            Arc::new(self.db.clone()),
            self.config.clone(),
            Arc::new(Factory(self.script.clone())),
            self.registry.clone(),
        )
    }
    fn service(&self) -> ToolExecutionService {
        ToolExecutionService::new(
            Arc::new(self.db.clone()),
            self.config.clone(),
            self.registry.clone(),
        )
    }
    fn route(&self, task: &str, steps: Vec<Step>) {
        self.script
            .routes
            .lock()
            .unwrap()
            .insert(task.into(), steps.into());
    }
    async fn root(&self) -> nuono_storage::entity::agent_runs::Model {
        self.runs()
            .await
            .into_iter()
            .find(|run| run.parent_id.is_none())
            .unwrap()
    }
    async fn runs(&self) -> Vec<nuono_storage::entity::agent_runs::Model> {
        nuono_storage::entity::agent_runs::Entity::find()
            .all(self.db.orm())
            .await
            .unwrap()
    }
    async fn paused(&self) -> nuono_storage::entity::agent_runs::Model {
        self.runs()
            .await
            .into_iter()
            .find(|row| row.status == "waiting_approval")
            .expect("missing paused run")
    }
}
fn context(text: &str) -> AgentContext {
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
        message_id: Some("message-1".into()),
        raw_message: text.into(),
        content: text.into(),
    }
}
fn owner() -> CallerIdentity {
    CallerIdentity::new(1, "owner", Role::Owner)
}
fn call(id: &str, name: &str) -> ChatToolCall {
    ChatToolCall {
        id: Some(id.into()),
        name: name.into(),
        arguments: json!({"key": id}),
    }
}
fn step(text: &str, calls: Vec<ChatToolCall>) -> Step {
    Step {
        response: ChatResponse {
            continuation: Vec::new(),
            id: "response".into(),
            model: "test-model".into(),
            content: text.into(),
            usage: UsageInfo {
                prompt_tokens: 20,
                completion_tokens: 10,
                total_tokens: 30,
            },
            finish_reason: Some(
                if calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }
                .into(),
            ),
            tool_calls: calls,
        },
        gate: None,
        panic_after_start: false,
    }
}

#[tokio::test]
async fn streamed_reply_only_answer_commits_the_normal_history_and_run_checkpoint() {
    use nuono_app::agent::output::OutputProgress;
    let h = Harness::new(vec![step("流式完整答案🙂", vec![])]).await;
    h.config.0.write().await.agent_autonomy = "reply_only".into();
    h.script.streaming.store(true, Ordering::SeqCst);
    let runtime = h.runtime();
    let ctx = context("work");
    let session = ctx.session_id();
    let (tx, rx) = tokio::sync::watch::channel(OutputProgress::default());
    let reply = runtime
        .submit_text_with_output(ctx, tx)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(reply.content, "流式完整答案🙂");
    assert!(rx.borrow().complete);
    assert_eq!(rx.borrow().content, reply.content);
    assert_eq!(h.root().await.status, "completed");
    assert!(
        h.db.recent_ai_conversations(&session, 100)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.role == "assistant" && entry.content.contains("流式完整答案🙂"))
    );
    assert_eq!(h.script.requests.lock().unwrap().len(), 1);
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn account_conversation_reuses_owned_history_and_memory_with_reply_only_access() {
    use nuono_app::agent::{ACCOUNT_CHAT_PLATFORM, store::AgentMemoryRecord};

    let h = Harness::new(vec![
        step("记下了", vec![]),
        step("你的项目叫雪松", vec![]),
        step("", vec![call("unexpected", "lookup")]),
    ])
    .await;
    let mut accounts = Vec::new();
    for name in ["private-alice", "private-bob"] {
        accounts.push(
            h.db.create_user(nuono_core::db::NewUser {
                username: name.into(),
                email: None,
                password: "Private-account-fixture-123!".into(),
                role: "user".into(),
                status: "active".into(),
                email_verified: false,
            })
            .await
            .unwrap()
            .id,
        );
    }
    {
        let mut cfg = h.config.0.write().await;
        cfg.agent_allowed_platforms = vec![ACCOUNT_CHAT_PLATFORM.into()];
        cfg.agent_memory_enabled = true;
        cfg.agent_memory_auto_extract_enabled = true;
    }
    h.db.upsert_agent_memory(AgentMemoryRecord {
        scope_type: "global".into(),
        scope_key: "global".into(),
        memory_key: "platform-private".into(),
        content: "project PLATFORM_ONLY".into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        actor: None,
    })
    .await
    .unwrap();
    let runtime = h.runtime();
    let mut spoof = context("spoofed platform");
    spoof.platform = ACCOUNT_CHAT_PLATFORM.into();
    assert!(runtime.submit_text(spoof).await.unwrap().is_none());
    assert!(
        runtime
            .submit_text(context("unrelated platform"))
            .await
            .unwrap()
            .is_none()
    );

    let send = |account, input: &str| {
        AgentContext::for_account_chat("fixture", account, input.into(), "负责个人对话".into())
    };
    let first = runtime
        .submit_text(send(accounts[0], "记住：project 雪松"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(first.content, "记下了");
    let second = runtime
        .submit_text(send(accounts[0], "project 是什么？"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(second.content, "你的项目叫雪松");
    let third = runtime
        .submit_text(send(accounts[1], "project 是什么？"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(third.content, "当前配置不允许工具调用");
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    {
        let requests = h.script.requests.lock().unwrap();
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.role == MessageRole::System
                    && message.content.contains("雪松"))
        );
        assert!(
            requests[1].messages.iter().any(
                |message| message.role == MessageRole::Assistant && message.content == "记下了"
            )
        );
        assert!(requests.iter().all(|request| {
            request
                .messages
                .iter()
                .all(|message| !message.content.contains("PLATFORM_ONLY"))
        }));
        assert!(
            requests[2]
                .messages
                .iter()
                .all(|message| !message.content.contains("雪松"))
        );
    }
    assert!(h.db.list_ai_conversations().await.unwrap().is_empty());
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

async fn wait_for_model(gate: &Gate) {
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("model did not start");
}

#[derive(Default)]
struct ToolProbe {
    active: AtomicUsize,
    peak: AtomicUsize,
    events: Mutex<Vec<String>>,
}
struct ActiveTool(Arc<ToolProbe>);
impl Drop for ActiveTool {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
struct CoordinatedTool {
    name: &'static str,
    risk: ToolRiskLevel,
    declaration: ToolScheduling,
    gates: HashMap<String, Arc<Gate>>,
    probe: Arc<ToolProbe>,
}
#[async_trait]
impl AgentTool for CoordinatedTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Observable tool with declared shared resources"
    }
    fn risk_level(&self) -> ToolRiskLevel {
        self.risk
    }
    fn scheduling(&self, _: &AgentContext, _: &Value) -> ToolScheduling {
        self.declaration.clone()
    }
    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }
    async fn call(&self, _: AgentToolContext, input: Value) -> Result<Value> {
        let id = input["key"].as_str().unwrap();
        let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveTool(self.probe.clone());
        self.probe.peak.fetch_max(active, Ordering::SeqCst);
        self.probe
            .events
            .lock()
            .unwrap()
            .push(format!("start:{id}"));
        if let Some(gate) = self.gates.get(id) {
            gate.entered.notify_one();
            gate.released.notified().await;
        }
        self.probe
            .events
            .lock()
            .unwrap()
            .push(format!("finish:{id}"));
        Ok(json!({"id":id}))
    }
}
fn register_coordinated(
    h: &Harness,
    name: &'static str,
    risk: ToolRiskLevel,
    declaration: ToolScheduling,
    gates: &[(&str, Arc<Gate>)],
    probe: &Arc<ToolProbe>,
) {
    h.registry.unregister(name);
    h.registry
        .register(CoordinatedTool {
            name,
            risk,
            declaration,
            gates: gates
                .iter()
                .map(|(id, gate)| ((*id).into(), gate.clone()))
                .collect(),
            probe: probe.clone(),
        })
        .unwrap();
}
async fn wait_queued<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn shared_reads_overlap_and_reverse_completions_keep_model_result_order() {
    let h = Harness::new(vec![
        step(
            "",
            vec![call("first", "parallel"), call("second", "parallel")],
        ),
        step("done", vec![]),
    ])
    .await;
    let first = Arc::new(Gate::default());
    let second = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "parallel",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::concurrent([ToolResource::read("records")]),
        &[("first", first.clone()), ("second", second.clone())],
        &probe,
    );
    let task = tokio::spawn(
        h.runtime()
            .submit_text(context("work"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&first).await;
    wait_for_model(&second).await;
    assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
    second.released.notify_one();
    // 先确认第二个任务确实执行完毕，再放行前一个阻塞中的模型调用。
    tokio::time::timeout(Duration::from_secs(5), async {
        while !probe
            .events
            .lock()
            .unwrap()
            .contains(&"finish:second".into())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished());
    first.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "done");
    let requests = h.script.requests.lock().unwrap();
    let results = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .map(|m| m.tool_call_id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        serde_json::from_str::<Value>(&results[0].content).unwrap()["output"]["id"],
        "first"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&results[1].content).unwrap()["output"]["id"],
        "second"
    );
}

#[tokio::test]
async fn conflicting_writes_and_approvals_are_barriers_for_later_parallel_calls() {
    let h = Harness::new(vec![
        step(
            "",
            vec![
                call("write", "writer"),
                call("a", "reader"),
                call("b", "reader"),
                call("approve", "change"),
                call("tail", "reader"),
            ],
        ),
        step("done", vec![]),
    ])
    .await;
    let write = Arc::new(Gate::default());
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "writer",
        ToolRiskLevel::LowRisk,
        ToolScheduling::concurrent([ToolResource::write("records")]),
        &[("write", write.clone())],
        &probe,
    );
    register_coordinated(
        &h,
        "reader",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::concurrent([ToolResource::read("records")]),
        &[("a", a.clone()), ("b", b.clone())],
        &probe,
    );
    // 高风险工具即使声明了互不冲突的资源，也依然需要人工审批拦截。
    register_coordinated(
        &h,
        "change",
        ToolRiskLevel::RequiresApproval,
        ToolScheduling::concurrent([ToolResource::write("other")]),
        &[],
        &probe,
    );
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&write).await;
    assert_eq!(*probe.events.lock().unwrap(), ["start:write"]);
    write.released.notify_one();
    wait_for_model(&a).await;
    wait_for_model(&b).await;
    a.released.notify_one();
    b.released.notify_one();
    task.await.unwrap().unwrap();
    let paused = h.paused().await;
    assert!(!probe.events.lock().unwrap().contains(&"start:tail".into()));
    assert!(
        !probe
            .events
            .lock()
            .unwrap()
            .contains(&"start:approve".into())
    );
    let id = paused.approval_id.unwrap();
    h.service().decide(&owner(), id, true, None).await.unwrap();
    assert_eq!(
        runtime.resume_approval(id).await.unwrap().unwrap().content,
        "done"
    );
    let events = probe.events.lock().unwrap();
    assert!(
        events.iter().position(|v| v == "finish:write").unwrap()
            < events.iter().position(|v| v == "start:a").unwrap()
    );
    assert!(
        events.iter().position(|v| v == "finish:approve").unwrap()
            < events.iter().position(|v| v == "start:tail").unwrap()
    );
    let requests = h.script.requests.lock().unwrap();
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .map(|m| m.tool_call_id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["write", "a", "b", "approve", "tail"]
    );
}

#[tokio::test]
async fn cancelling_parallel_wave_drops_all_tool_futures_and_releases_resources() {
    let h = Harness::new(vec![step(
        "",
        vec![
            call("done", "parallel"),
            call("a", "parallel"),
            call("b", "parallel"),
            call("later", "lookup"),
        ],
    )])
    .await;
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "parallel",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::concurrent([ToolResource::read("records")]),
        &[("a", a.clone()), ("b", b.clone())],
        &probe,
    );
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&a).await;
    wait_for_model(&b).await;
    // 同一批次中已完成的并列工具调用，即使同批其余任务被取消，也应保留其已持久化的执行结果。
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let receipts = nuono_storage::entity::agent_tool_receipts::Entity::find()
                .all(h.db.orm())
                .await
                .unwrap();
            if receipts.iter().any(|receipt| {
                receipt.status == "Succeeded"
                    && receipt.input_json.as_deref() == Some("{\"key\":\"done\"}")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let run = h.runs().await.pop().unwrap();
    runtime.cancel_run(&run.id).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.content, "任务已取消");
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    assert!(
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v.starts_with("finish:"))
            .eq([&"finish:done".to_string()])
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    // 默认独占的工具在取消恢复后，必须同时拿到全局互斥锁和并发槽位才能执行。
    let execution = tokio::time::timeout(
        Duration::from_secs(5),
        h.service().execute(
            context("new"),
            nuono_app::agent::AutonomyLevel::LowRiskTools,
            "lookup",
            json!({}),
            "new call",
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(execution.status, ToolExecutionStatus::Succeeded);
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn queued_service_calls_recheck_policy_and_reject_replaced_tools() {
    let h = Harness::new(vec![]).await;
    let gate = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "holder",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::Exclusive,
        &[("hold", gate.clone())],
        &probe,
    );
    let registry = h.registry.clone();
    let holder = tokio::spawn(async move {
        registry
            .execute(
                "holder",
                nuono_app::agent::AutonomyLevel::LowRiskTools,
                AgentToolContext::new(context("hold")),
                json!({"key":"hold"}),
            )
            .await
    });
    wait_for_model(&gate).await;
    let service = h.service();
    let mut denied = Box::pin(service.execute(
        context("denied"),
        nuono_app::agent::AutonomyLevel::LowRiskTools,
        "lookup",
        json!({}),
        "queued call",
    ));
    let mut replaced = Box::pin(service.execute(
        context("replaced"),
        nuono_app::agent::AutonomyLevel::ApprovedTools,
        "change",
        json!({}),
        "queued call",
    ));
    wait_queued(denied.as_mut()).await;
    wait_queued(replaced.as_mut()).await;
    h.config
        .0
        .write()
        .await
        .agent_denied_tools
        .push("lookup".into());
    let new_effects = Arc::new(AtomicUsize::new(0));
    h.registry.unregister("change");
    h.registry
        .register(Counter {
            name: "change",
            calls: new_effects.clone(),
            contexts: h.contexts.clone(),
            risk: ToolRiskLevel::LowRisk,
        })
        .unwrap();
    gate.released.notify_one();
    holder.await.unwrap();
    assert_eq!(denied.await.unwrap().status, ToolExecutionStatus::Rejected);
    let result = replaced.await.unwrap();
    assert_eq!(result.status, ToolExecutionStatus::Rejected);
    assert!(result.output["error"].as_str().unwrap().contains("已变更"));
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert_eq!(new_effects.load(Ordering::SeqCst), 0);
    assert!(
        h.db.list_agent_tool_approvals(None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn claimed_approval_waiting_for_resource_cannot_execute_after_run_cancellation() {
    let h = Harness::new(vec![step("", vec![call("change", "change")])]).await;
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let run = h.paused().await;
    let gate = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "holder",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::Exclusive,
        &[("hold", gate.clone())],
        &probe,
    );
    let registry = h.registry.clone();
    let holder = tokio::spawn(async move {
        registry
            .execute(
                "holder",
                nuono_app::agent::AutonomyLevel::LowRiskTools,
                AgentToolContext::new(context("hold")),
                json!({"key":"hold"}),
            )
            .await
    });
    wait_for_model(&gate).await;
    let service = h.service();
    let approval_id = run.approval_id.unwrap();
    let decision =
        tokio::spawn(async move { service.decide(&owner(), approval_id, true, None).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while h
            .db
            .get_agent_tool_approval(approval_id)
            .await
            .unwrap()
            .unwrap()
            .status
            != "executing"
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.cancel_run(&run.id).await.unwrap();
    gate.released.notify_one();
    holder.await.unwrap();
    assert_eq!(decision.await.unwrap().unwrap().status, "execute_failed");
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert!(
        runtime
            .resume_approval(approval_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn invalid_resource_declarations_fail_before_any_tool_effect() {
    let h = Harness::new(vec![
        step("", vec![call("invalid", "invalid")]),
        step("declaration rejected", vec![]),
    ])
    .await;
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "invalid",
        ToolRiskLevel::LowRisk,
        ToolScheduling::concurrent([ToolResource::write("")]),
        &[],
        &probe,
    );
    assert_eq!(
        h.runtime()
            .handle_text(context("work"))
            .await
            .unwrap()
            .content,
        "declaration rejected"
    );
    assert!(probe.events.lock().unwrap().is_empty());
    let requests = h.script.requests.lock().unwrap();
    let result = requests[1]
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["status"],
        "Rejected"
    );
}

#[tokio::test]
async fn parallel_dispatch_respects_global_cap_and_remaining_call_budget() {
    let mut h = Harness::new(vec![step(
        "",
        (0..5).map(|i| call(&i.to_string(), "parallel")).collect(),
    )])
    .await;
    h.registry = Arc::new(ToolRegistry::with_concurrency_limit(2).unwrap());
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "parallel",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::concurrent([]),
        &[("0", a.clone()), ("1", b.clone())],
        &probe,
    );
    let runtime = h.runtime().with_run_limits(RunLimits {
        max_tool_calls: 3,
        ..RunLimits::default()
    });
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&a).await;
    wait_for_model(&b).await;
    assert_eq!(probe.active.load(Ordering::SeqCst), 2);
    assert_eq!(probe.events.lock().unwrap().len(), 2);
    a.released.notify_one();
    b.released.notify_one();
    let reply = task.await.unwrap().unwrap();
    let run = runtime.get_run(&reply.run_id).await.unwrap().unwrap();
    assert_eq!(run.status, "budget_exceeded");
    assert_eq!(run.tool_calls, 3);
    assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v.starts_with("finish:"))
            .count(),
        3
    );
}

#[tokio::test]
async fn consumes_whole_batches_and_keeps_tools_available_on_later_rounds() {
    let h = Harness::new(vec![
        step("", vec![call("a", "lookup"), call("b", "lookup")]),
        step("", vec![call("c", "lookup")]),
        step("done", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    assert_eq!(
        runtime.handle_text(context("work")).await.unwrap().content,
        "done"
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 3);
    {
        let requests = h.script.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.tools.len() == 3));
        let results = requests[2]
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .map(|m| m.tool_call_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        for message in results {
            assert_eq!(
                serde_json::from_str::<Value>(&message.content).unwrap()["status"],
                "Succeeded"
            );
        }
    }
    let run = h.runs().await.pop().unwrap();
    let view = runtime.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(
        (
            view.status.as_str(),
            view.rounds,
            view.tool_calls,
            view.tokens
        ),
        ("completed", 3, 3, 90)
    );
}

#[tokio::test]
async fn approved_checkpoint_survives_runtime_recreation_and_never_reexecutes() {
    let h = Harness::new(vec![
        step("", vec![call("write", "change"), call("read", "lookup")]),
        step("finished", vec![]),
    ])
    .await;
    h.runtime().handle_text(context("work")).await.unwrap();
    let run = h.paused().await;
    let id = run.approval_id.unwrap();
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    let resumed = h.runtime();
    resumed
        .steer_run(&run.id, "also explain the result")
        .await
        .unwrap();
    let service = h.service();
    let caller = owner();
    let (first, duplicate) = tokio::join!(
        service.decide(&caller, id, true, None),
        service.decide(&caller, id, true, None)
    );
    first.unwrap();
    duplicate.unwrap();
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    let other = h.runtime();
    let (one, two) = tokio::join!(resumed.resume_approval(id), other.resume_approval(id));
    assert_eq!(
        usize::from(one.as_ref().is_ok_and(|reply| reply.is_some()))
            + usize::from(two.as_ref().is_ok_and(|reply| reply.is_some())),
        1
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        resumed
            .get_run(&run.id)
            .await
            .unwrap()
            .unwrap()
            .reply
            .as_deref(),
        Some("finished")
    );
    service.decide(&owner(), id, true, None).await.unwrap();
    assert!(resumed.resume_approval(id).await.unwrap().is_none());
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    let approval = h.db.get_agent_tool_approval(id).await.unwrap().unwrap();
    assert!(approval.execution_json.is_some());
    let requests = h.script.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert_eq!(
        last.messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .map(|m| m.tool_call_id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["write", "read"]
    );
    assert_eq!(
        last.messages.last().unwrap().content,
        "also explain the result"
    );
}

#[tokio::test]
async fn rejected_approval_is_a_tool_result_and_allows_a_followup_answer() {
    let h = Harness::new(vec![
        step("", vec![call("write", "change")]),
        step("request was declined", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let id = h.paused().await.approval_id.unwrap();
    h.service().decide(&owner(), id, false, None).await.unwrap();
    assert_eq!(
        runtime.resume_approval(id).await.unwrap().unwrap().content,
        "request was declined"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    let requests = h.script.requests.lock().unwrap();
    let result = requests[1]
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert_eq!(result.tool_call_id.as_deref(), Some("write"));
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["status"],
        "rejected"
    );
}

#[tokio::test]
async fn cancel_paused_run_blocks_approval_dispatch_and_frees_session() {
    let h = Harness::new(vec![
        step("", vec![call("write", "change")]),
        step("new task", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let run = h.paused().await;
    let id = run.approval_id.unwrap();
    runtime.cancel_run(&run.id).await.unwrap();
    assert!(h.service().decide(&owner(), id, true, None).await.is_err());
    // 即使审批通过与任务取消发生并发竞态，取消后该审批也不能再抢到执行权。
    h.db.decide_agent_tool_approval(id, true, Some("owner".into()), None)
        .await
        .unwrap();
    assert!(
        h.db.claim_agent_tool_approval_for_execution(id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(runtime.resume_approval(id).await.unwrap().is_none());
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.get_run(&run.id).await.unwrap().unwrap().status,
        "cancelled"
    );
    assert_eq!(
        runtime
            .handle_text(context("start again"))
            .await
            .unwrap()
            .content,
        "new task"
    );
}

#[tokio::test]
async fn steering_during_final_inference_gets_another_turn_and_other_bots_are_independent() {
    let gate = Arc::new(Gate::default());
    let mut blocked = step("draft", vec![]);
    blocked.gate = Some(gate.clone());
    let h = Harness::new(vec![
        blocked,
        step("other bot", vec![]),
        step("updated answer", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    let task_runtime = runtime.clone();
    let task =
        tokio::spawn(async move { task_runtime.handle_text(context("work")).await.unwrap() });
    wait_for_model(&gate).await;
    let mut other_context = context("separate work");
    other_context.bot_id = 9;
    assert_eq!(
        runtime.handle_text(other_context).await.unwrap().content,
        "other bot"
    );
    assert!(
        runtime
            .handle_text(context("include the followup"))
            .await
            .unwrap()
            .content
            .contains("补充消息")
    );
    gate.released.notify_one();
    assert_eq!(task.await.unwrap().content, "updated answer");
    {
        let requests = h.script.requests.lock().unwrap();
        let last = requests.last().unwrap();
        assert_eq!(
            last.messages.last().unwrap().content,
            "include the followup"
        );
        assert!(
            last.messages
                .iter()
                .any(|m| m.role == MessageRole::Assistant && m.content == "draft")
        );
    }
    assert_eq!(h.runs().await.len(), 2);
}

#[tokio::test]
async fn cancellation_interrupts_inference_before_any_tool_dispatch() {
    let gate = Arc::new(Gate::default());
    let mut blocked = step("", vec![call("unused", "lookup")]);
    blocked.gate = Some(gate.clone());
    let h = Harness::new(vec![blocked, step("new task", vec![])]).await;
    let runtime = h.runtime();
    let task_runtime = runtime.clone();
    let task =
        tokio::spawn(async move { task_runtime.handle_text(context("work")).await.unwrap() });
    wait_for_model(&gate).await;
    let run = h.runs().await.pop().unwrap();
    runtime.cancel_run(&run.id).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.content, "任务已取消");
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.get_run(&run.id).await.unwrap().unwrap().status,
        "cancelled"
    );
    assert_eq!(
        runtime.handle_text(context("new")).await.unwrap().content,
        "new task"
    );
}

#[tokio::test]
async fn round_budget_stops_infinite_tool_loop() {
    let h = Harness::new(vec![
        step("", vec![call("one", "lookup")]),
        step("", vec![call("two", "lookup")]),
    ])
    .await;
    let runtime = h.runtime().with_run_limits(RunLimits {
        max_rounds: 2,
        ..RunLimits::default()
    });
    assert!(
        runtime
            .handle_text(context("work"))
            .await
            .unwrap()
            .content
            .contains("轮数上限")
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 2);
    let run = h.runs().await.pop().unwrap();
    assert_eq!(
        runtime.get_run(&run.id).await.unwrap().unwrap().status,
        "budget_exceeded"
    );
}

#[tokio::test]
async fn policy_is_rechecked_before_approved_tool_executes() {
    let h = Harness::new(vec![
        step("", vec![call("write", "change")]),
        step("blocked by current policy", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let id = h.paused().await.approval_id.unwrap();
    h.config
        .0
        .write()
        .await
        .agent_denied_tools
        .push("change".into());
    let approval = h.service().decide(&owner(), id, true, None).await.unwrap();
    assert_eq!(approval.status, "execute_failed");
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    runtime.resume_approval(id).await.unwrap().unwrap();
    let requests = h.script.requests.lock().unwrap();
    assert!(requests[1].tools.iter().all(|tool| tool.name != "change"));
    let result = requests[1]
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["status"],
        "Rejected"
    );
    assert!(h.contexts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_identity_is_preserved_through_approval_and_continuation() {
    use nuono_app::agent::AgentNativeContext;
    use nuono_domain::{
        BotIdentity, ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform,
    };
    let h = Harness::new(vec![
        step("", vec![call("write", "change")]),
        step("native reply", vec![]),
    ])
    .await;
    let mut ctx = context("work");
    let bot = BotIdentity::new(
        "qq-instance-A",
        IdentityRef::new(
            Platform::QqOfficial,
            IdentityKind::Bot,
            IdentityScheme::Native,
            "app-not-a-number",
            None::<String>,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let actor = IdentityRef::new(
        Platform::QqOfficial,
        IdentityKind::User,
        IdentityScheme::OpenId,
        "Exact_OpenId",
        Some("app-not-a-number"),
    )
    .unwrap();
    ctx.bot_id = 0;
    ctx.user_id = 0;
    ctx.platform = "qq_official".into();
    ctx.native = Some(AgentNativeContext {
        bot,
        actor: actor.clone(),
        conversation: Some(ConversationRef::Direct {
            participant: actor,
            conversation: None,
        }),
        mentioned: false,
    });
    let expected = serde_json::to_value(&ctx).unwrap();
    h.runtime().handle_text(ctx).await.unwrap();
    let id = h.paused().await.approval_id.unwrap();
    h.service().decide(&owner(), id, true, None).await.unwrap();
    let reply = h.runtime().resume_approval(id).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(reply.context).unwrap(), expected);
    assert_eq!(
        serde_json::to_value(&h.contexts.lock().unwrap()[0]).unwrap(),
        expected
    );
}

#[tokio::test]
async fn submission_releases_ingress_and_timeout_leaves_no_tool_effects() {
    let gate = Arc::new(Gate::default());
    let mut blocked = step("", vec![call("unused", "lookup")]);
    blocked.gate = Some(gate.clone());
    let h = Harness::new(vec![blocked]).await;
    let runtime = h.runtime().with_run_limits(RunLimits {
        timeout: Duration::from_millis(100),
        ..RunLimits::default()
    });
    let task = tokio::time::timeout(Duration::from_secs(5), runtime.submit_text(context("work")))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(h.script.requests.lock().unwrap().is_empty());
    let task = tokio::spawn(task);
    wait_for_model(&gate).await;
    let reply = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime
            .get_run(&reply.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "timed_out"
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_finds_approval_even_before_pause_checkpoint_was_written() {
    let h = Harness::new(vec![step("", vec![call("write", "change")])]).await;
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let row = h.paused().await;
    let id = row.approval_id.unwrap();
    assert_eq!(
        h.db.get_agent_tool_approval(id)
            .await
            .unwrap()
            .unwrap()
            .run_id
            .as_deref(),
        Some(row.id.as_str())
    );
    // 模拟进程在写入审批记录后、尚未把 approval_id 回写到运行记录前发生崩溃中断。
    let mut interrupted: nuono_app::agent::store::AgentRunRecord = row.into();
    let revision = interrupted.revision;
    interrupted.status = "running".into();
    interrupted.approval_id = None;
    h.db.update_agent_run(interrupted.clone(), revision)
        .await
        .unwrap();
    runtime.cancel_run(&interrupted.id).await.unwrap();
    assert!(h.service().decide(&owner(), id, true, None).await.is_err());
    h.db.decide_agent_tool_approval(id, true, Some("owner".into()), None)
        .await
        .unwrap();
    assert!(
        h.db.claim_agent_tool_approval_for_execution(id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn repeated_first_approval_cannot_consume_a_later_approval_in_the_same_batch() {
    let h = Harness::new(vec![
        step(
            "",
            vec![
                call("first", "change"),
                call("second", "change"),
                call("read", "lookup"),
            ],
        ),
        step("done", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let first = h.paused().await.approval_id.unwrap();
    h.service()
        .decide(&owner(), first, true, None)
        .await
        .unwrap();
    runtime.resume_approval(first).await.unwrap().unwrap();
    let second = h.paused().await.approval_id.unwrap();
    assert_ne!(first, second);
    h.service()
        .decide(&owner(), first, true, None)
        .await
        .unwrap();
    assert!(runtime.resume_approval(first).await.unwrap().is_none());
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    h.service()
        .decide(&owner(), second, true, None)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .resume_approval(second)
            .await
            .unwrap()
            .unwrap()
            .content,
        "done"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 2);
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
}

fn delegate(id: &str, tasks: Value) -> ChatToolCall {
    ChatToolCall {
        id: Some(id.into()),
        name: "agent.delegate".into(),
        arguments: json!({"tasks":tasks}),
    }
}
fn gated_step(text: &str, gate: &Arc<Gate>) -> Step {
    let mut step = step(text, vec![]);
    step.gate = Some(gate.clone());
    step
}

#[tokio::test]
async fn children_run_in_parallel_without_holding_tool_slots_and_return_ordered_results() {
    let mut h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([
                    {"task":"child-a", "title":"A"}, {"task":"child-b", "title":"B"}
                ]),
            )],
        ),
        step("parent summary", vec![]),
    ])
    .await;
    // 父级委派调用不能一直占着唯一的工具并发槽位，否则子任务将无法执行工具。
    h.registry = Arc::new(ToolRegistry::with_concurrency_limit(1).unwrap());
    h.registry
        .register(Counter {
            name: "lookup",
            risk: ToolRiskLevel::ReadOnly,
            calls: h.reads.clone(),
            contexts: h.contexts.clone(),
        })
        .unwrap();
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    h.route(
        "child-a",
        vec![
            step("", vec![call("read", "lookup")]),
            gated_step("evidence A", &a),
        ],
    );
    h.route("child-b", vec![gated_step("evidence B", &b)]);
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&a).await;
    wait_for_model(&b).await;
    let root = h.root().await;
    let children = h.db.list_agent_children(&root.id).await.unwrap();
    assert_eq!(children.len(), 2);
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(view.children.len(), 2);
    for child in &children {
        assert_ne!(child.session_id, root.session_id);
        assert_eq!(child.parent_id.as_deref(), Some(root.id.as_str()));
        assert_eq!(
            runtime
                .get_run(&child.id)
                .await
                .unwrap()
                .unwrap()
                .limits
                .max_rounds,
            8
        );
    }
    b.released.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runtime
                .get_run(&root.id)
                .await
                .unwrap()
                .unwrap()
                .children
                .iter()
                .any(|child| child.title == "B" && child.status == "completed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished());
    a.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "parent summary");
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
    let history =
        h.db.recent_ai_conversations(&context("work").session_id(), 100)
            .await
            .unwrap();
    assert!(
        history
            .iter()
            .all(|entry| !entry.content.contains("evidence"))
    );
    let requests = h.script.requests.lock().unwrap();
    let offered = &requests[0].tools;
    let delegation = offered
        .iter()
        .find(|tool| tool.name == "agent.delegate")
        .unwrap();
    let mut host_names = offered
        .iter()
        .filter(|tool| tool.name != "agent.delegate")
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    host_names.sort();
    assert_eq!(
        delegation.parameters_schema["properties"]["tasks"]["items"]["properties"]["tools"]["items"]
            ["enum"],
        json!(host_names)
    );
    let last = requests.last().unwrap();
    let result = last
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("fork"))
        .unwrap();
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["output"]["children"][0]["content"], "evidence A");
    assert_eq!(output["output"]["children"][1]["content"], "evidence B");
    for request in requests.iter().filter(|request| {
        request.messages.iter().any(|message| {
            message.role == MessageRole::User && message.content.starts_with("child-")
        })
    }) {
        assert!(
            !request
                .tools
                .iter()
                .any(|tool| tool.name == "agent.delegate")
        );
        assert!(
            !request
                .messages
                .iter()
                .any(|message| message.role == MessageRole::User && message.content == "work")
        );
    }
}

#[tokio::test]
async fn readonly_children_cannot_call_ungranted_tools_or_create_grandchildren() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate("fork", json!([{"task":"readonly-child"}]))],
        ),
        step("parent done", vec![]),
    ])
    .await;
    h.route(
        "readonly-child",
        vec![
            step(
                "",
                vec![
                    call("forbidden-write", "change"),
                    delegate("nested", json!([{"task":"grandchild"}])),
                ],
            ),
            step("child done", vec![]),
        ],
    );
    assert_eq!(
        h.runtime()
            .handle_text(context("work"))
            .await
            .unwrap()
            .content,
        "parent done"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert!(
        h.db.list_agent_tool_approvals(None, 20)
            .await
            .unwrap()
            .is_empty()
    );
    let rows = h.runs().await;
    assert_eq!(rows.len(), 2);
    let child = rows.iter().find(|run| run.parent_id.is_some()).unwrap();
    assert!(
        h.db.list_agent_children(&child.id)
            .await
            .unwrap()
            .is_empty()
    );
    let requests = h.script.requests.lock().unwrap();
    let child_request = requests
        .iter()
        .find(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content == "readonly-child")
        })
        .unwrap();
    assert_eq!(
        child_request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["lookup"]
    );
    let final_child = requests
        .iter()
        .rfind(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content == "readonly-child")
        })
        .unwrap();
    let results = final_child
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .map(|message| serde_json::from_str::<Value>(&message.content).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result["status"] == "Rejected"));
}

#[tokio::test]
async fn child_approvals_resume_parent_after_recreation_without_expanding_saved_authority() {
    let h = Harness::new(vec![step("", vec![delegate("fork", json!([{
        "task":"approved-child", "tools":["change"], "budget":{"max_rounds":3,"max_tool_calls":4,"max_tokens":100,"timeout_secs":5}
    }]))]), step("parent completed", vec![])]).await;
    h.route(
        "approved-child",
        vec![
            step("", vec![call("first", "change")]),
            step("", vec![call("second", "change")]),
            step("child completed", vec![]),
        ],
    );
    let runtime = h.runtime().with_run_limits(RunLimits {
        max_rounds: 4,
        timeout: Duration::from_secs(10),
        ..RunLimits::default()
    });
    let paused = runtime
        .submit_text(context("work"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let root = runtime.get_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(root.status, "waiting_children");
    let first = root.children[0].approval_id.unwrap();
    let child_id = root.children[0].id.clone();
    assert_eq!(
        h.runtime()
            .get_run(&child_id)
            .await
            .unwrap()
            .unwrap()
            .limits
            .max_rounds,
        3
    );
    runtime
        .steer_run(&root.id, "include the child evidence")
        .await
        .unwrap();
    h.config.0.write().await.agent_autonomy = "admin_approved".into();
    h.service()
        .decide(&owner(), first, true, None)
        .await
        .unwrap();
    let resumed = h.runtime().resume_approval(first).await.unwrap().unwrap();
    assert_eq!(resumed.run_id, root.id);
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    let waiting = h.runtime().get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(waiting.status, "waiting_children");
    assert_eq!(waiting.limits.max_rounds, 4);
    let second = waiting.children[0].approval_id.unwrap();
    assert_ne!(second, first);
    assert!(resumed.content.contains(&format!("#{second}")));
    assert!(
        waiting
            .reply
            .as_deref()
            .unwrap()
            .contains(&format!("#{second}"))
    );
    assert!(h.runtime().resume_approval(first).await.unwrap().is_none());
    h.service()
        .decide(&owner(), second, true, None)
        .await
        .unwrap();
    let reply = h.runtime().resume_approval(second).await.unwrap().unwrap();
    assert_eq!(reply.run_id, root.id);
    assert_eq!(reply.content, "parent completed");
    assert_eq!(h.effects.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.runtime()
            .get_run(&root.id)
            .await
            .unwrap()
            .unwrap()
            .tool_calls,
        1
    );
    assert!(h.runtime().resume_approval(second).await.unwrap().is_none());
    assert!(
        h.runtime()
            .collect_children(&root.id)
            .await
            .unwrap()
            .is_none()
    );
    let requests = h.script.requests.lock().unwrap();
    assert_eq!(
        requests.last().unwrap().messages.last().unwrap().content,
        "include the child evidence"
    );
}

#[tokio::test]
async fn parent_cancellation_stops_all_active_children_and_frees_admission_slots() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([{"task":"child-a"},{"task":"child-b"}]),
            )],
        ),
        step("new task", vec![]),
    ])
    .await;
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    let mut first = step("", vec![call("unused-a", "lookup")]);
    first.gate = Some(a.clone());
    let mut second = step("", vec![call("unused-b", "lookup")]);
    second.gate = Some(b.clone());
    h.route("child-a", vec![first]);
    h.route("child-b", vec![second]);
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&a).await;
    wait_for_model(&b).await;
    let root = h.root().await;
    runtime.cancel_run(&root.id).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.content, "任务已取消");
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(view.status, "cancelled");
    assert!(
        view.children
            .iter()
            .all(|child| child.status == "cancelled"),
        "parent cancellation left child states: {:?}",
        view.children
            .iter()
            .map(|child| (&child.id, &child.status))
            .collect::<Vec<_>>()
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.handle_text(context("new")).await.unwrap().content,
        "new task"
    );
}

#[tokio::test]
async fn parent_cancellation_blocks_child_approval_claim_even_before_child_rows_are_cancelled() {
    let h = Harness::new(vec![step(
        "",
        vec![delegate(
            "fork",
            json!([{"task":"approval-child", "tools":["change"]}]),
        )],
    )])
    .await;
    h.route(
        "approval-child",
        vec![step("", vec![call("write", "change")])],
    );
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let child =
        h.db.list_agent_children(&root.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
    let id = child.approval_id.unwrap();
    // 模拟父任务已提交取消状态、但尚未级联更新到已暂停子任务检查点的中断场景。
    let mut stopped: nuono_app::agent::store::AgentRunRecord = root.into();
    let revision = stopped.revision;
    stopped.status = "cancelled".into();
    h.db.update_agent_run(stopped.clone(), revision)
        .await
        .unwrap();
    h.db.decide_agent_tool_approval(id, true, Some("owner".into()), None)
        .await
        .unwrap();
    assert!(
        h.db.claim_agent_tool_approval_for_execution(id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    runtime.cancel_run(&stopped.id).await.unwrap();
    assert_eq!(
        runtime.get_run(&child.id).await.unwrap().unwrap().status,
        "cancelled"
    );
}

#[tokio::test]
async fn child_budget_exhaustion_is_collected_without_stopping_successful_siblings() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([
                    {"task":"bounded-child", "budget":{"max_rounds":1}}, {"task":"healthy-child"}
                ]),
            )],
        ),
        step("parent handles partial result", vec![]),
    ])
    .await;
    h.route(
        "bounded-child",
        vec![step("", vec![call("read", "lookup")])],
    );
    h.route("healthy-child", vec![step("good result", vec![])]);
    let reply = h
        .runtime()
        .submit_text(context("work"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(reply.content, "parent handles partial result");
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
    let requests = h.script.requests.lock().unwrap();
    let tool = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("fork"))
        .unwrap();
    let result: Value = serde_json::from_str(&tool.content).unwrap();
    assert_eq!(result["output"]["children"][0]["status"], "budget_exceeded");
    assert_eq!(result["output"]["children"][1]["content"], "good result");
}

#[tokio::test]
async fn invalid_child_requests_never_create_a_partial_batch() {
    for invalid in [
        json!({"task":"invalid", "tools":["forbidden"]}),
        json!({"task":"invalid", "budget":{"max_rounds":999}}),
    ] {
        let h = Harness::new(vec![
            step(
                "",
                vec![delegate("fork", json!([{"task":"valid"}, invalid]))],
            ),
            step("request rejected", vec![]),
        ])
        .await;
        assert_eq!(
            h.runtime()
                .handle_text(context("work"))
                .await
                .unwrap()
                .content,
            "request rejected"
        );
        assert_eq!(h.runs().await.len(), 1);
        assert_eq!(h.script.requests.lock().unwrap().len(), 2);
        let root = h.root().await;
        let checkpoint: Value = serde_json::from_str(&root.state_json).unwrap();
        assert_eq!(checkpoint["children_spawned"], 0);
        assert!(checkpoint["delegation"].is_null());
    }
}

#[tokio::test]
async fn child_admission_cap_is_shared_across_parent_runs() {
    let h = Harness::new(vec![]).await;
    h.route(
        "parent-a",
        vec![
            step("", vec![delegate("fork-a", json!([{"task":"child-a"}]))]),
            step("parent A", vec![]),
        ],
    );
    h.route(
        "parent-b",
        vec![
            step("", vec![delegate("fork-b", json!([{"task":"child-b"}]))]),
            step("parent B", vec![]),
        ],
    );
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    h.route("child-a", vec![gated_step("A", &a)]);
    h.route("child-b", vec![gated_step("B", &b)]);
    let runtime = h
        .runtime()
        .with_sub_agent_limits(SubAgentLimits {
            max_concurrent: 1,
            ..SubAgentLimits::default()
        })
        .unwrap();
    let first = tokio::spawn(
        runtime
            .submit_text(context("parent-a"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&a).await;
    let mut other = context("parent-b");
    other.user_id = 3;
    let second = tokio::spawn(runtime.submit_text(other).await.unwrap().unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.runs().await.len() == 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!h.script.requests.lock().unwrap().iter().any(|request| {
        request
            .messages
            .iter()
            .any(|message| message.role == MessageRole::User && message.content == "child-b")
    }));
    a.released.notify_one();
    wait_for_model(&b).await;
    b.released.notify_one();
    assert_eq!(first.await.unwrap().unwrap().content, "parent A");
    assert_eq!(second.await.unwrap().unwrap().content, "parent B");
}

#[tokio::test]
async fn parent_waits_for_every_child_approval_and_consumes_each_result_once() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([
                    {"task":"child-a", "tools":["change"]}, {"task":"child-b", "tools":["change"]}
                ]),
            )],
        ),
        step("both complete", vec![]),
    ])
    .await;
    for name in ["child-a", "child-b"] {
        h.route(
            name,
            vec![step("", vec![call(name, "change")]), step(name, vec![])],
        );
    }
    let runtime = h.runtime();
    let root_reply = runtime
        .submit_text(context("work"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let children = runtime
        .get_run(&root_reply.run_id)
        .await
        .unwrap()
        .unwrap()
        .children;
    let first = children[0].approval_id.unwrap();
    let second = children[1].approval_id.unwrap();
    h.service()
        .decide(&owner(), first, true, None)
        .await
        .unwrap();
    let waiting = runtime.resume_approval(first).await.unwrap().unwrap();
    assert_eq!(waiting.run_id, root_reply.run_id);
    assert_eq!(
        runtime
            .get_run(&root_reply.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "waiting_children"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    assert!(!h.script.requests.lock().unwrap().iter().any(|request| {
        request
            .messages
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some("fork"))
    }));
    h.service()
        .decide(&owner(), second, true, None)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .resume_approval(second)
            .await
            .unwrap()
            .unwrap()
            .content,
        "both complete"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 2);
    assert!(
        runtime
            .collect_children(&root_reply.run_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delegation_creation_rolls_back_children_and_parent_revision_on_failure() {
    let gate = Arc::new(Gate::default());
    let h = Harness::new(vec![gated_step("done", &gate)]).await;
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&gate).await;
    let root: nuono_app::agent::store::AgentRunRecord = h.root().await.into();
    let child = nuono_app::agent::store::AgentRunRecord {
        id: "rollback-child".into(),
        parent_id: Some(root.id.clone()),
        session_id: "child:rollback".into(),
        status: "running".into(),
        state_json: root.state_json.clone(),
        approval_id: None,
        revision: 0,
    };
    assert!(
        !h.db
            .create_agent_children(root.clone(), root.revision + 1, vec![child.clone()])
            .await
            .unwrap()
    );
    assert!(h.db.list_agent_children(&root.id).await.unwrap().is_empty());
    assert!(
        h.db.create_agent_children(root.clone(), root.revision, vec![child.clone(), child])
            .await
            .is_err()
    );
    assert!(h.db.list_agent_children(&root.id).await.unwrap().is_empty());
    let unchanged = h.db.get_agent_run(&root.id).await.unwrap().unwrap();
    assert_eq!(unchanged.revision, root.revision);
    assert_eq!(unchanged.state_json, root.state_json);
    gate.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "done");
}

#[tokio::test]
async fn parent_lifetime_child_limit_blocks_later_delegations() {
    let h = Harness::new(vec![
        step("", vec![delegate("first", json!([{"task":"allowed"}]))]),
        step("", vec![delegate("second", json!([{"task":"too-many"}]))]),
        step("bounded parent", vec![]),
    ])
    .await;
    h.route("allowed", vec![step("done", vec![])]);
    let runtime = h
        .runtime()
        .with_sub_agent_limits(SubAgentLimits {
            max_children: 1,
            ..SubAgentLimits::default()
        })
        .unwrap();
    assert_eq!(
        runtime.handle_text(context("work")).await.unwrap().content,
        "bounded parent"
    );
    assert_eq!(h.runs().await.len(), 2);
    let requests = h.script.requests.lock().unwrap();
    let denied = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("second"))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&denied.content).unwrap()["status"],
        "Rejected"
    );
}

#[tokio::test]
async fn tool_risk_changes_cannot_expand_a_childs_saved_grant() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate("fork", json!([{"task":"snapshot-child"}]))],
        ),
        step("safe parent", vec![]),
    ])
    .await;
    let gate = Arc::new(Gate::default());
    let mut changed = step("", vec![call("read", "lookup")]);
    changed.gate = Some(gate.clone());
    h.route("snapshot-child", vec![changed, step("safe child", vec![])]);
    let task = tokio::spawn(
        h.runtime()
            .submit_text(context("work"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&gate).await;
    h.registry.unregister("lookup");
    let effects = Arc::new(AtomicUsize::new(0));
    h.registry
        .register(Counter {
            name: "lookup",
            calls: effects.clone(),
            contexts: h.contexts.clone(),
            risk: ToolRiskLevel::LowRisk,
        })
        .unwrap();
    gate.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "safe parent");
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    let requests = h.script.requests.lock().unwrap();
    let child_final = requests
        .iter()
        .rfind(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content == "snapshot-child")
        })
        .unwrap();
    assert!(child_final.tools.is_empty());
}

#[tokio::test]
async fn cancelling_parent_blocks_an_already_claimed_child_approval_queued_for_tool_resources() {
    let h = Harness::new(vec![step(
        "",
        vec![delegate(
            "fork",
            json!([{"task":"approval-child", "tools":["change"]}]),
        )],
    )])
    .await;
    h.route(
        "approval-child",
        vec![step("", vec![call("write", "change")])],
    );
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let id = h.paused().await.approval_id.unwrap();
    let gate = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    register_coordinated(
        &h,
        "holder",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::Exclusive,
        &[("hold", gate.clone())],
        &probe,
    );
    let registry = h.registry.clone();
    let holder = tokio::spawn(async move {
        registry
            .execute(
                "holder",
                nuono_app::agent::AutonomyLevel::LowRiskTools,
                AgentToolContext::new(context("hold")),
                json!({"key":"hold"}),
            )
            .await
    });
    wait_for_model(&gate).await;
    let service = h.service();
    let decision = tokio::spawn(async move { service.decide(&owner(), id, true, None).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.db
                .get_agent_tool_approval(id)
                .await
                .unwrap()
                .unwrap()
                .status
                == "executing"
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.cancel_run(&root.id).await.unwrap();
    gate.released.notify_one();
    holder.await.unwrap();
    assert_eq!(decision.await.unwrap().unwrap().status, "execute_failed");
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert!(runtime.resume_approval(id).await.unwrap().is_none());
}

#[tokio::test]
async fn collecting_parent_never_replays_an_interrupted_running_child() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([{"task":"interrupted-child", "tools":["change"]}]),
            )],
        ),
        step("parent reports cancellation", vec![]),
    ])
    .await;
    h.route(
        "interrupted-child",
        vec![step("", vec![call("write", "change")])],
    );
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let mut child =
        h.db.list_agent_children(&root.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
    let revision = child.revision;
    child.status = "running".into();
    child.approval_id = None;
    h.db.update_agent_run(child.clone(), revision)
        .await
        .unwrap();
    assert!(runtime.collect_children(&root.id).await.unwrap().is_none());
    assert_eq!(h.script.requests.lock().unwrap().len(), 2);
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.get_run(&root.id).await.unwrap().unwrap().status,
        "waiting_children"
    );
    runtime.cancel_run(&child.id).await.unwrap();
    assert_eq!(
        runtime
            .collect_children(&root.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "parent reports cancellation"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
}

fn request_task(request: &ChatRequest) -> &str {
    request
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.as_str())
        .unwrap_or_default()
}

fn task_requests(h: &Harness, task: &str) -> Vec<ChatRequest> {
    h.script
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request_task(request) == task)
        .cloned()
        .collect()
}

fn dependency_input(request: &ChatRequest) -> Option<Value> {
    request
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
        .find_map(|value| value.get("dependency_results").cloned())
}

async fn persist_completed_child(h: &Harness, id: &str, content: &str) {
    // 模拟子任务已完成持久化，但父任务还没来得及汇总结果进程就退出的情况。
    let mut child = h.db.get_agent_run(id).await.unwrap().unwrap();
    let revision = child.revision;
    let mut checkpoint: Value = serde_json::from_str(&child.state_json).unwrap();
    checkpoint["reply"] = json!(content);
    checkpoint["pending"] = json!([]);
    child.state_json = serde_json::to_string(&checkpoint).unwrap();
    child.status = "completed".into();
    child.approval_id = None;
    assert!(h.db.update_agent_run(child, revision).await.unwrap());
}

#[tokio::test]
async fn dag_diamond_runs_ready_branches_and_injects_only_ordered_direct_dependencies() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "graph",
                json!([
                    {"id":"a", "task":"dag-source"},
                    {"id":"b", "task":"dag-left", "depends_on":["a"]},
                    {"id":"c", "task":"dag-right", "depends_on":["a"]},
                    {"id":"join", "task":"dag-join", "depends_on":["c","b"]},
                    {"id":"other", "task":"dag-other"}
                ]),
            )],
        ),
        step("graph summary", vec![]),
    ])
    .await;
    let source = Arc::new(Gate::default());
    let left = Arc::new(Gate::default());
    let right = Arc::new(Gate::default());
    h.route("dag-source", vec![gated_step("source evidence", &source)]);
    h.route("dag-left", vec![gated_step("left evidence", &left)]);
    h.route("dag-right", vec![gated_step("right evidence", &right)]);
    h.route("dag-join", vec![step("joined evidence", vec![])]);
    h.route("dag-other", vec![step("unrelated evidence", vec![])]);
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&source).await;
    let root = h.root().await;
    let before = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(before.children.len(), 5);
    for node in ["b", "c", "join"] {
        let child = before
            .children
            .iter()
            .find(|child| child.node_id.as_deref() == Some(node))
            .unwrap();
        assert_eq!(child.status, "queued");
        assert_eq!(child.rounds, 0);
    }
    assert!(task_requests(&h, "dag-left").is_empty());
    assert!(task_requests(&h, "dag-right").is_empty());
    source.released.notify_one();
    wait_for_model(&left).await;
    wait_for_model(&right).await;
    right.released.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = runtime.get_run(&root.id).await.unwrap().unwrap();
            if current
                .children
                .iter()
                .any(|child| child.node_id.as_deref() == Some("c") && child.status == "completed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(task_requests(&h, "dag-join").is_empty());
    left.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "graph summary");
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert!(
        view.children
            .iter()
            .all(|child| child.status == "completed")
    );
    let node_run = |node: &str| {
        view.children
            .iter()
            .find(|child| child.node_id.as_deref() == Some(node))
            .unwrap()
            .id
            .clone()
    };
    let join = view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("join"))
        .unwrap();
    assert_eq!(join.depends_on, vec![node_run("c"), node_run("b")]);
    for task_name in ["dag-source", "dag-other"] {
        let requests = task_requests(&h, task_name);
        assert_eq!(requests.len(), 1);
        assert!(dependency_input(&requests[0]).is_none());
    }
    for task_name in ["dag-left", "dag-right"] {
        let requests = task_requests(&h, task_name);
        assert_eq!(requests.len(), 1);
        let evidence = dependency_input(&requests[0]).unwrap();
        assert_eq!(evidence.as_array().unwrap().len(), 1);
        assert_eq!(evidence[0]["node_id"], "a");
        assert_eq!(evidence[0]["run_id"], node_run("a"));
        assert_eq!(evidence[0]["content"], "source evidence");
    }
    let requests = task_requests(&h, "dag-join");
    assert_eq!(requests.len(), 1);
    let evidence = dependency_input(&requests[0]).unwrap();
    assert_eq!(evidence.as_array().unwrap().len(), 2);
    assert_eq!(evidence[0]["node_id"], "c");
    assert_eq!(evidence[0]["run_id"], node_run("c"));
    assert_eq!(evidence[0]["content"], "right evidence");
    assert_eq!(evidence[1]["node_id"], "b");
    assert_eq!(evidence[1]["run_id"], node_run("b"));
    assert_eq!(evidence[1]["content"], "left evidence");
    for result in evidence.as_array().unwrap() {
        assert_eq!(result["status"], "completed");
        for field in ["rounds", "tool_calls", "tokens", "active_ms"] {
            assert!(result[field].is_u64(), "missing dependency usage {field}");
        }
    }
    let parent_requests = task_requests(&h, "work");
    let result = parent_requests
        .last()
        .unwrap()
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("graph"))
        .unwrap();
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["output"]["children"].as_array().unwrap().len(), 5);
    assert_eq!(
        output["output"]["children"][0]["content"],
        "source evidence"
    );
    assert_eq!(
        output["output"]["children"][3]["content"],
        "joined evidence"
    );
}

#[tokio::test]
async fn dag_eight_independent_nodes_respect_shared_four_inference_slots() {
    let tasks = (1..=8)
        .map(|index| json!({"task":format!("dag-wide-{index}")}))
        .collect::<Vec<_>>();
    let h = Harness::new(vec![
        step("", vec![delegate("wide", json!(tasks))]),
        step("all eight complete", vec![]),
    ])
    .await;
    let gates = (1..=8)
        .map(|index| {
            let name = format!("dag-wide-{index}");
            let gate = Arc::new(Gate::default());
            h.route(&name, vec![gated_step(&name, &gate)]);
            (name, gate)
        })
        .collect::<HashMap<_, _>>();
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let started = h
                .script
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request_task(request).starts_with("dag-wide-"))
                .map(|request| request_task(request).to_string())
                .collect::<Vec<_>>();
            if started.len() >= 4 {
                break started;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(first.len(), 4);
    let root = h.root().await;
    assert_eq!(h.db.list_agent_children(&root.id).await.unwrap().len(), 8);
    let requests = task_requests(&h, "work");
    let declaration = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "agent.delegate")
        .unwrap();
    assert_eq!(
        declaration.parameters_schema["properties"]["tasks"]["maxItems"],
        8
    );
    for name in &first {
        wait_for_model(gates.get(name).unwrap()).await;
        gates[name].released.notify_one();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let started = h
                .script
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request_task(request).starts_with("dag-wide-"))
                .count();
            if started == 8 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for (name, gate) in &gates {
        if !first.contains(name) {
            wait_for_model(gate).await;
            gate.released.notify_one();
        }
    }
    assert_eq!(task.await.unwrap().unwrap().content, "all eight complete");
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    let mut ids = view
        .children
        .iter()
        .map(|child| child.node_id.clone().unwrap())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(
        ids,
        (1..=8)
            .map(|index| format!("task_{index}"))
            .collect::<Vec<_>>()
    );
    assert!(
        view.children
            .iter()
            .all(|child| child.status == "completed")
    );
    assert_eq!(h.script.requests.lock().unwrap().len(), 10);
}

#[tokio::test]
async fn invalid_dags_are_rejected_atomically_before_any_child_creation_or_inference() {
    for tasks in [
        json!([{"id":"same","task":"valid"},{"id":"same","task":"duplicate"}]),
        json!([{"id":"a","task":"valid"},{"id":"b","task":"duplicate-edge","depends_on":["a","a"]}]),
        json!([{"id":"a","task":"valid"},{"id":"b","task":"unknown","depends_on":["missing"]}]),
        json!([{"id":"a","task":"self","depends_on":["a"]}]),
        json!([{"id":"a","task":"cycle-a","depends_on":["b"]},{"id":"b","task":"cycle-b","depends_on":["a"]}]),
        json!([{"id":"a","task":"cycle-a","depends_on":["c"]},{"id":"b","task":"cycle-b","depends_on":["a"]},{"id":"c","task":"cycle-c","depends_on":["b"]}]),
        json!([{"id":"task_2","task":"explicit"},{"task":"implicit-collision"}]),
        json!([{"id":"invalid key","task":"invalid-key"}]),
        json!([{"id":"中文","task":"nonascii-key"}]),
        json!(
            (1..=9)
                .map(|index| json!({"task":format!("too-many-{index}")}))
                .collect::<Vec<_>>()
        ),
    ] {
        let h = Harness::new(vec![
            step("", vec![delegate("invalid-graph", tasks)]),
            step("invalid graph rejected", vec![]),
        ])
        .await;
        assert_eq!(
            h.runtime()
                .handle_text(context("work"))
                .await
                .unwrap()
                .content,
            "invalid graph rejected"
        );
        let root = h.root().await;
        assert_eq!(h.runs().await.len(), 1);
        assert!(h.db.list_agent_children(&root.id).await.unwrap().is_empty());
        let checkpoint: Value = serde_json::from_str(&root.state_json).unwrap();
        assert_eq!(checkpoint["children_spawned"], 0);
        assert!(checkpoint["delegation"].is_null());
        let requests = task_requests(&h, "work");
        assert_eq!(requests.len(), 2);
        let result = requests[1]
            .messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("invalid-graph"))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap()["status"],
            "Rejected"
        );
    }
}

#[tokio::test]
async fn dag_failed_dependency_blocks_transitive_successors_but_keeps_independent_branch_running() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "partial",
                json!([
                    {"id":"bounded", "task":"dag-bounded", "budget":{"max_rounds":1}},
                    {"id":"blocked", "task":"dag-blocked", "depends_on":["bounded"]},
                    {"id":"transitive", "task":"dag-transitive", "depends_on":["blocked"]},
                    {"id":"healthy", "task":"dag-healthy"},
                    {"id":"healthy-next", "task":"dag-healthy-next", "depends_on":["healthy"]}
                ]),
            )],
        ),
        step("partial graph summarized", vec![]),
    ])
    .await;
    h.route(
        "dag-bounded",
        vec![step("", vec![call("bounded-read", "lookup")])],
    );
    h.route("dag-blocked", vec![]);
    h.route("dag-transitive", vec![]);
    h.route("dag-healthy", vec![step("healthy evidence", vec![])]);
    h.route("dag-healthy-next", vec![step("healthy followup", vec![])]);
    let runtime = h.runtime();
    assert_eq!(
        runtime.handle_text(context("work")).await.unwrap().content,
        "partial graph summarized"
    );
    let root = h.root().await;
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    for (node, status) in [
        ("bounded", "budget_exceeded"),
        ("blocked", "blocked"),
        ("transitive", "blocked"),
        ("healthy", "completed"),
        ("healthy-next", "completed"),
    ] {
        let child = view
            .children
            .iter()
            .find(|child| child.node_id.as_deref() == Some(node))
            .unwrap();
        assert_eq!(child.status, status, "incorrect outcome for {node}");
        if status == "blocked" {
            assert_eq!(child.rounds, 0);
            assert_eq!(child.tool_calls, 0);
            assert_eq!(child.tokens, 0);
        }
    }
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
    assert!(task_requests(&h, "dag-blocked").is_empty());
    assert!(task_requests(&h, "dag-transitive").is_empty());
    let requests = task_requests(&h, "dag-healthy-next");
    let evidence = dependency_input(&requests[0]).unwrap();
    assert_eq!(evidence.as_array().unwrap().len(), 1);
    assert_eq!(evidence[0]["node_id"], "healthy");
    assert_eq!(evidence[0]["content"], "healthy evidence");
}

#[tokio::test]
async fn dag_approval_resume_after_recreation_starts_successors_with_original_budget_and_grants() {
    let h = Harness::new(vec![
        step("", vec![delegate("approved-graph", json!([
            {"id":"approval", "task":"dag-approval-source", "tools":["change"]},
            {"id":"successor", "task":"dag-approval-successor", "depends_on":["approval"], "tools":["lookup","change"], "budget":{"max_rounds":2,"max_tool_calls":3,"max_tokens":100,"timeout_secs":5}}
        ]))]),
        step("approved graph complete", vec![]),
    ]).await;
    h.route(
        "dag-approval-source",
        vec![
            step("", vec![call("source-write", "change")]),
            step("approved source evidence", vec![]),
        ],
    );
    h.route(
        "dag-approval-successor",
        vec![
            step(
                "",
                vec![
                    call("changed-risk", "lookup"),
                    call("successor-write", "change"),
                ],
            ),
            step("successor evidence", vec![]),
        ],
    );
    let initial = h.runtime().with_run_limits(RunLimits {
        max_rounds: 4,
        timeout: Duration::from_secs(10),
        ..RunLimits::default()
    });
    initial.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let view = initial.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(view.status, "waiting_children");
    let source = view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("approval"))
        .unwrap();
    let source_id = source.id.clone();
    let first = source.approval_id.unwrap();
    let successor = view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("successor"))
        .unwrap();
    let successor_id = successor.id.clone();
    assert_eq!(successor.status, "queued");
    assert_eq!(successor.depends_on, vec![source_id]);
    assert!(task_requests(&h, "dag-approval-successor").is_empty());
    h.config.0.write().await.agent_autonomy = "admin_approved".into();
    h.registry.unregister("lookup");
    h.registry
        .register(Counter {
            name: "lookup",
            calls: h.reads.clone(),
            contexts: h.contexts.clone(),
            risk: ToolRiskLevel::LowRisk,
        })
        .unwrap();
    h.service()
        .decide(&owner(), first, true, None)
        .await
        .unwrap();
    let recreated = h.runtime();
    let waiting = recreated.resume_approval(first).await.unwrap().unwrap();
    assert_eq!(waiting.run_id, root.id);
    let view = recreated.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(view.status, "waiting_children");
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    let successor = recreated.get_run(&successor_id).await.unwrap().unwrap();
    assert_eq!(successor.status, "waiting_approval");
    assert_eq!(successor.limits.max_rounds, 2);
    assert_eq!(successor.limits.max_tool_calls, 3);
    assert_eq!(successor.limits.max_tokens, 100);
    assert_eq!(successor.limits.timeout, Duration::from_secs(5));
    let second = successor.approval_id.unwrap();
    assert_ne!(first, second);
    let requests = task_requests(&h, "dag-approval-successor");
    assert_eq!(requests.len(), 1);
    assert_eq!(request_task(&requests[0]), "dag-approval-successor");
    assert!(requests[0].tools.iter().all(|tool| tool.name != "lookup"));
    let evidence = dependency_input(&requests[0]).unwrap();
    assert_eq!(evidence[0]["node_id"], "approval");
    assert_eq!(evidence[0]["content"], "approved source evidence");
    assert!(recreated.resume_approval(first).await.unwrap().is_none());
    h.service()
        .decide(&owner(), second, true, None)
        .await
        .unwrap();
    let reply = h.runtime().resume_approval(second).await.unwrap().unwrap();
    assert_eq!(reply.run_id, root.id);
    assert_eq!(reply.content, "approved graph complete");
    assert_eq!(h.effects.load(Ordering::SeqCst), 2);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    assert!(
        h.runtime()
            .collect_children(&root.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cancelling_dag_parent_cancels_unstarted_successors_and_invalidates_source_approval() {
    let h = Harness::new(vec![step(
        "",
        vec![delegate(
            "cancel-graph",
            json!([
                {"id":"source", "task":"dag-cancel-source", "tools":["change"]},
                {"id":"next", "task":"dag-cancel-next", "depends_on":["source"]},
                {"id":"last", "task":"dag-cancel-last", "depends_on":["next"]}
            ]),
        )],
    )])
    .await;
    h.route(
        "dag-cancel-source",
        vec![step("", vec![call("cancel-write", "change")])],
    );
    h.route("dag-cancel-next", vec![]);
    h.route("dag-cancel-last", vec![]);
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let before = runtime.get_run(&root.id).await.unwrap().unwrap();
    let approval = before
        .children
        .iter()
        .find_map(|child| child.approval_id)
        .unwrap();
    assert_eq!(
        before
            .children
            .iter()
            .filter(|child| child.status == "queued")
            .count(),
        2
    );
    runtime.cancel_run(&root.id).await.unwrap();
    let after = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(after.status, "cancelled");
    assert!(
        after
            .children
            .iter()
            .all(|child| child.status == "cancelled")
    );
    assert!(
        h.service()
            .decide(&owner(), approval, true, None)
            .await
            .is_err()
    );
    assert!(runtime.resume_approval(approval).await.unwrap().is_none());
    assert!(runtime.collect_children(&root.id).await.unwrap().is_none());
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert!(task_requests(&h, "dag-cancel-next").is_empty());
    assert!(task_requests(&h, "dag-cancel-last").is_empty());
}

#[tokio::test]
async fn concurrent_dag_collectors_claim_ready_successor_once_across_runtime_instances() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "collect-graph",
                json!([
                    {"id":"source", "task":"dag-collect-source", "tools":["change"]},
                    {"id":"next", "task":"dag-collect-next", "depends_on":["source"]}
                ]),
            )],
        ),
        step("collected once", vec![]),
    ])
    .await;
    h.route(
        "dag-collect-source",
        vec![step("", vec![call("source-write", "change")])],
    );
    let gate = Arc::new(Gate::default());
    let mut blocked = step("", vec![call("next-read", "lookup")]);
    blocked.gate = Some(gate.clone());
    h.route(
        "dag-collect-next",
        vec![blocked, step("next evidence", vec![])],
    );
    h.runtime().handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let view = h.runtime().get_run(&root.id).await.unwrap().unwrap();
    let source_id = &view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("source"))
        .unwrap()
        .id;
    persist_completed_child(&h, source_id, "durable source evidence").await;
    let first_runtime = h.runtime();
    let second_runtime = h.runtime();
    let first_id = root.id.clone();
    let second_id = root.id.clone();
    let first = tokio::spawn(async move { first_runtime.collect_children(&first_id).await });
    let second = tokio::spawn(async move { second_runtime.collect_children(&second_id).await });
    wait_for_model(&gate).await;
    assert_eq!(task_requests(&h, "dag-collect-next").len(), 1);
    gate.released.notify_one();
    let results = tokio::time::timeout(Duration::from_secs(5), async {
        vec![
            first.await.unwrap().unwrap(),
            second.await.unwrap().unwrap(),
        ]
    })
    .await
    .unwrap();
    let replies = results.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].run_id, root.id);
    assert_eq!(replies[0].content, "collected once");
    assert_eq!(h.reads.load(Ordering::SeqCst), 1);
    assert_eq!(task_requests(&h, "dag-collect-next").len(), 2);
    assert_eq!(task_requests(&h, "work").len(), 2);
    assert!(
        h.runtime()
            .collect_children(&root.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn dag_collector_does_not_restart_claimed_child_or_release_its_successor() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "interrupted-graph",
                json!([
                    {"id":"source", "task":"dag-interrupted-source", "tools":["change"]},
                    {"id":"claimed", "task":"dag-interrupted-claimed", "depends_on":["source"]},
                    {"id":"after", "task":"dag-interrupted-after", "depends_on":["claimed"]}
                ]),
            )],
        ),
        step("interruption summarized", vec![]),
    ])
    .await;
    h.route(
        "dag-interrupted-source",
        vec![step("", vec![call("source-write", "change")])],
    );
    h.route("dag-interrupted-claimed", vec![]);
    h.route("dag-interrupted-after", vec![]);
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    let source = view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("source"))
        .unwrap();
    persist_completed_child(&h, &source.id, "source evidence").await;
    let claimed = view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("claimed"))
        .unwrap();
    let mut record = h.db.get_agent_run(&claimed.id).await.unwrap().unwrap();
    let revision = record.revision;
    // 模拟抢占到执行权的工作协程在推理前崩溃；已持久化的 running 状态不应被直接重放。
    record.status = "running".into();
    assert!(h.db.update_agent_run(record, revision).await.unwrap());
    assert!(
        h.runtime()
            .collect_children(&root.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.script.requests.lock().unwrap().len(), 2);
    assert!(task_requests(&h, "dag-interrupted-claimed").is_empty());
    assert!(task_requests(&h, "dag-interrupted-after").is_empty());
    runtime.cancel_run(&claimed.id).await.unwrap();
    assert_eq!(
        h.runtime()
            .collect_children(&root.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "interruption summarized"
    );
    let after = runtime.get_run(&root.id).await.unwrap().unwrap();
    let blocked = after
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("after"))
        .unwrap();
    assert_eq!(blocked.status, "blocked");
    assert_eq!(blocked.rounds, 0);
    assert!(task_requests(&h, "dag-interrupted-after").is_empty());
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dag_forward_dependencies_resolve_default_node_keys_before_execution() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "forward-graph",
                json!([
                    {"task":"dag-forward-join", "depends_on":["task_2"]},
                    {"task":"dag-forward-source"},
                    {"id":"other", "task":"dag-forward-other"}
                ]),
            )],
        ),
        step("forward graph complete", vec![]),
    ])
    .await;
    let gate = Arc::new(Gate::default());
    h.route(
        "dag-forward-source",
        vec![gated_step("forward source evidence", &gate)],
    );
    h.route("dag-forward-other", vec![step("unrelated", vec![])]);
    h.route("dag-forward-join", vec![step("forward joined", vec![])]);
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&gate).await;
    assert!(task_requests(&h, "dag-forward-join").is_empty());
    let root = h.root().await;
    let before = runtime.get_run(&root.id).await.unwrap().unwrap();
    let source = before
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("task_2"))
        .unwrap();
    let join = before
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("task_1"))
        .unwrap();
    assert_eq!(join.depends_on, vec![source.id.clone()]);
    assert_eq!(join.status, "queued");
    gate.released.notify_one();
    assert_eq!(
        task.await.unwrap().unwrap().content,
        "forward graph complete"
    );
    let requests = task_requests(&h, "dag-forward-join");
    assert_eq!(requests.len(), 1);
    let evidence = dependency_input(&requests[0]).unwrap();
    assert_eq!(evidence.as_array().unwrap().len(), 1);
    assert_eq!(evidence[0]["node_id"], "task_2");
    assert_eq!(evidence[0]["content"], "forward source evidence");
    let requests = task_requests(&h, "work");
    let result = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("forward-graph"))
        .unwrap();
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["output"]["children"][0]["content"], "forward joined");
    assert_eq!(
        output["output"]["children"][1]["content"],
        "forward source evidence"
    );
}

#[tokio::test]
async fn dag_child_claim_cas_requires_current_running_parent_and_allows_only_one_winner() {
    let h = Harness::new(vec![step(
        "",
        vec![delegate(
            "claim-graph",
            json!([
                {"id":"source", "task":"dag-claim-source", "tools":["change"]},
                {"id":"next", "task":"dag-claim-next", "depends_on":["source"]},
                {"id":"last", "task":"dag-claim-last", "depends_on":["next"]}
            ]),
        )],
    )])
    .await;
    h.route(
        "dag-claim-source",
        vec![step("", vec![call("source-write", "change")])],
    );
    h.route("dag-claim-next", vec![]);
    h.route("dag-claim-last", vec![]);
    let runtime = h.runtime();
    runtime.handle_text(context("work")).await.unwrap();
    let root = h.root().await;
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    let next_id = &view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("next"))
        .unwrap()
        .id;
    let last_id = &view
        .children
        .iter()
        .find(|child| child.node_id.as_deref() == Some("last"))
        .unwrap()
        .id;
    let mut next = h.db.get_agent_run(next_id).await.unwrap().unwrap();
    let next_revision = next.revision;
    next.status = "running".into();
    // 处于等待子任务状态的父运行记录，即使版本号匹配也不能直接授权恢复执行。
    assert!(
        !h.db
            .claim_agent_child(next.clone(), next_revision, root.revision)
            .await
            .unwrap()
    );
    let mut parent: nuono_app::agent::store::AgentRunRecord = root.into();
    let paused_revision = parent.revision;
    parent.status = "running".into();
    assert!(
        h.db.update_agent_run(parent.clone(), paused_revision)
            .await
            .unwrap()
    );
    parent = h.db.get_agent_run(&parent.id).await.unwrap().unwrap();
    assert!(
        !h.db
            .claim_agent_child(next.clone(), next_revision, paused_revision)
            .await
            .unwrap()
    );
    let (first, second) = tokio::join!(
        h.db.claim_agent_child(next.clone(), next_revision, parent.revision),
        h.db.claim_agent_child(next.clone(), next_revision, parent.revision),
    );
    assert_eq!(
        usize::from(first.unwrap()) + usize::from(second.unwrap()),
        1
    );
    let claimed = h.db.get_agent_run(next_id).await.unwrap().unwrap();
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.revision, next_revision + 1);
    assert!(
        !h.db
            .claim_agent_child(next, next_revision, parent.revision)
            .await
            .unwrap()
    );
    let mut last = h.db.get_agent_run(last_id).await.unwrap().unwrap();
    let last_revision = last.revision;
    last.status = "running".into();
    let parent_revision = parent.revision;
    parent.status = "cancelled".into();
    assert!(
        h.db.update_agent_run(parent, parent_revision)
            .await
            .unwrap()
    );
    assert!(
        !h.db
            .claim_agent_child(last, last_revision, parent_revision + 1)
            .await
            .unwrap()
    );
    assert_eq!(
        h.db.get_agent_run(last_id).await.unwrap().unwrap().status,
        "queued"
    );
    assert!(task_requests(&h, "dag-claim-next").is_empty());
    assert!(task_requests(&h, "dag-claim-last").is_empty());
}

#[tokio::test]
async fn dag_approved_source_starts_successor_while_an_independent_branch_remains_live() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "live-graph",
                json!([
                    {"id":"approval", "task":"dag-live-approval", "tools":["change"]},
                    {"id":"successor", "task":"dag-live-successor", "depends_on":["approval"]},
                    {"id":"independent", "task":"dag-live-independent"}
                ]),
            )],
        ),
        step("live graph complete", vec![]),
    ])
    .await;
    let independent = Arc::new(Gate::default());
    let successor = Arc::new(Gate::default());
    h.route(
        "dag-live-approval",
        vec![
            step("", vec![call("source-write", "change")]),
            step("approved evidence", vec![]),
        ],
    );
    h.route(
        "dag-live-successor",
        vec![gated_step("successor evidence", &successor)],
    );
    h.route(
        "dag-live-independent",
        vec![gated_step("independent evidence", &independent)],
    );
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&independent).await;
    let root = h.root().await;
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = runtime.get_run(&root.id).await.unwrap().unwrap();
            if let Some(id) = view.children.iter().find_map(|child| child.approval_id) {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(task_requests(&h, "dag-live-successor").is_empty());
    h.service()
        .decide(&owner(), approval, true, None)
        .await
        .unwrap();
    assert!(
        h.runtime()
            .resume_approval(approval)
            .await
            .unwrap()
            .is_none()
    );
    wait_for_model(&successor).await;
    assert!(!task.is_finished());
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(view.status, "running");
    assert!(view.children.iter().any(|child| {
        child.node_id.as_deref() == Some("independent") && child.status == "running"
    }));
    let requests = task_requests(&h, "dag-live-successor");
    let evidence = dependency_input(&requests[0]).unwrap();
    assert_eq!(evidence.as_array().unwrap().len(), 1);
    assert_eq!(evidence[0]["node_id"], "approval");
    assert_eq!(evidence[0]["content"], "approved evidence");
    successor.released.notify_one();
    independent.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "live graph complete");
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    assert_eq!(task_requests(&h, "dag-live-successor").len(), 1);
}

#[tokio::test]
async fn oversized_dag_dependency_input_fails_join_without_losing_large_child_results() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "large-graph",
                json!([
                    {"id":"large-a", "task":"dag-large-a"},
                    {"id":"large-b", "task":"dag-large-b"},
                    {"id":"large-c", "task":"dag-large-c"},
                    {"id":"large-d", "task":"dag-large-d"},
                    {"id":"join", "task":"dag-large-join", "depends_on":["large-a","large-b","large-c","large-d"]},
                    {"id":"after", "task":"dag-large-after", "depends_on":["join"]},
                    {"id":"small", "task":"dag-large-independent"}
                ]),
            )],
        ),
        step("large results summarized", vec![]),
    ])
    .await;
    // 每个前置任务自身的输出加上消息历史都在单个检查点的大小上限内，
    // 但四个成功完成的前置依赖合并后超过了汇聚节点 1 MiB 的检查点上限。
    let contents = [
        ("large-a", "A".repeat(300_000)),
        ("large-b", "B".repeat(300_000)),
        ("large-c", "C".repeat(300_000)),
        ("large-d", "D".repeat(300_000)),
    ];
    for (node, content) in &contents {
        h.route(&format!("dag-{node}"), vec![step(content, vec![])]);
    }
    h.route("dag-large-join", vec![]);
    h.route("dag-large-after", vec![]);
    h.route(
        "dag-large-independent",
        vec![step("small independent result", vec![])],
    );
    let runtime = h.runtime();
    let reply = runtime.handle_text(context("work")).await.unwrap();
    assert_eq!(reply.content, "large results summarized");
    let root = h.root().await;
    let view = runtime.get_run(&root.id).await.unwrap().unwrap();
    assert_eq!(view.status, "completed");
    for (node, expected) in &contents {
        let child = view
            .children
            .iter()
            .find(|child| child.node_id.as_deref() == Some(*node))
            .unwrap();
        assert_eq!(child.status, "completed");
        assert!(child.reply.as_deref() == Some(expected.as_str()));
        let individual = runtime.get_run(&child.id).await.unwrap().unwrap();
        assert!(individual.reply.as_deref() == Some(expected.as_str()));
        let saved = h.db.get_agent_run(&child.id).await.unwrap().unwrap();
        assert!(saved.state_json.len() < 1_048_576);
        let checkpoint: Value = serde_json::from_str(&saved.state_json).unwrap();
        assert!(checkpoint["reply"].as_str() == Some(expected.as_str()));
    }
    for (node, status) in [
        ("join", "failed"),
        ("after", "blocked"),
        ("small", "completed"),
    ] {
        let child = view
            .children
            .iter()
            .find(|child| child.node_id.as_deref() == Some(node))
            .unwrap();
        assert_eq!(child.status, status);
        if node != "small" {
            assert_eq!(child.rounds, 0);
            assert_eq!(child.tool_calls, 0);
        }
    }
    assert!(task_requests(&h, "dag-large-join").is_empty());
    assert!(task_requests(&h, "dag-large-after").is_empty());
    let requests = task_requests(&h, "work");
    assert_eq!(requests.len(), 2);
    let result = requests[1]
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("large-graph"))
        .unwrap();
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "Succeeded");
    let children = output["output"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 7);
    for node in ["large-a", "large-b", "large-c", "large-d"] {
        let child = children
            .iter()
            .find(|child| child["node_id"] == node)
            .unwrap();
        assert_eq!(child["content_truncated"], true);
        assert!(child["content"].as_str().unwrap().len() < 300_000);
    }
    assert!(root.state_json.len() < 1_048_576);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn root_run_list_orders_updates_then_ids_and_filters_before_limiting() {
    use nuono_app::agent::store::AgentRunRecord;
    use nuono_storage::entity::agent_runs;
    use sea_orm::{ActiveModelTrait, Set};

    let h = Harness::new(vec![step("template complete", vec![])]).await;
    let runtime = h.runtime();
    let template_id = runtime
        .submit_text(context("template"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap()
        .run_id;
    let template = h.db.get_agent_run(&template_id).await.unwrap().unwrap();
    let old = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let latest = old + chrono::Duration::seconds(10);
    agent_runs::ActiveModel {
        id: Set(template_id),
        updated_at: Set(nuono_core::clock::store_time(
            old - chrono::Duration::seconds(10),
        )),
        ..Default::default()
    }
    .update(h.db.orm())
    .await
    .unwrap();

    for (id, status, updated_at) in [
        ("root-a", "waiting_approval", old),
        ("root-b", "waiting_approval", latest),
        ("root-c", "completed", latest),
    ] {
        h.db.create_agent_run(AgentRunRecord {
            id: id.into(),
            session_id: format!("list:{id}"),
            status: status.into(),
            ..template.clone()
        })
        .await
        .unwrap();
        agent_runs::ActiveModel {
            id: Set(id.into()),
            updated_at: Set(nuono_core::clock::store_time(updated_at)),
            ..Default::default()
        }
        .update(h.db.orm())
        .await
        .unwrap();
    }
    for id in ["child-a", "child-z"] {
        h.db.create_agent_run(AgentRunRecord {
            id: id.into(),
            parent_id: Some("root-c".into()),
            session_id: format!("list:{id}"),
            status: "completed".into(),
            ..template.clone()
        })
        .await
        .unwrap();
    }
    let page = runtime.list_runs(None, 2).await.unwrap();
    assert_eq!(
        page.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
        ["root-c", "root-b"]
    );
    assert_eq!(page[0].child_count, 2);
    assert_eq!(page[1].child_count, 0);
    assert_eq!(page[0].updated_at, latest.timestamp_millis().to_string());
    let filtered = runtime
        .list_runs(Some("waiting_approval"), 1)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, "root-b");
    let all = runtime.list_runs(None, 100).await.unwrap();
    assert_eq!(all.len(), 4);
    assert!(all.iter().all(|run| !run.id.starts_with("child-")));
    assert!(
        runtime
            .list_runs(Some("failed"), 50)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn root_run_list_has_unicode_previews_and_never_exposes_checkpoints() {
    let h = Harness::new(vec![step("done", vec![])]).await;
    let runtime = h.runtime();
    let root_id = runtime
        .submit_text(context("seed"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap()
        .run_id;
    let mut row = h.db.get_agent_run(&root_id).await.unwrap().unwrap();
    let task = format!("{}\nprivate task detail", "测🧪".repeat(70));
    let reply = format!("{}private reply suffix", "答🌸".repeat(100));
    let mut state: Value = serde_json::from_str(&row.state_json).unwrap();
    state["context"]["content"] = json!(task);
    state["context"]["platform"] = json!("telegram");
    state["context"]["bot_id"] = json!(11);
    state["messages"] = json!([{"role":"system", "content":"private system prompt"}]);
    state["pending"] = json!([{"id":"private-call", "name":"lookup", "arguments":{"secret":"private tool input"}}]);
    state["reply"] = json!(reply);
    state["rounds"] = json!(3);
    state["tool_calls"] = json!(5);
    state["tokens"] = json!(700);
    row.state_json = state.to_string();
    row.approval_id = Some(42);
    assert!(
        h.db.update_agent_run(row.clone(), row.revision)
            .await
            .unwrap()
    );

    let runs = runtime.list_runs(None, 50).await.unwrap();
    assert_eq!(runs.len(), 1);
    let summary = &runs[0];
    assert_eq!(summary.title, "测🧪".repeat(48));
    assert_eq!(summary.title.chars().count(), 96);
    assert_eq!(summary.reply_preview, Some("答🌸".repeat(80)));
    assert_eq!(summary.platform, "telegram");
    assert_eq!(summary.bot_id, 11);
    assert_eq!(
        (summary.rounds, summary.tool_calls, summary.tokens),
        (3, 5, 700)
    );
    assert_eq!(summary.approval_id, Some(42));
    let serialized = serde_json::to_value(summary).unwrap();
    assert_eq!(serialized.as_object().unwrap().len(), 12);
    for field in [
        "state_json",
        "messages",
        "pending",
        "task",
        "reply",
        "context",
    ] {
        assert!(serialized.get(field).is_none(), "list leaked {field}");
    }
    assert!(!serialized.to_string().contains("private"));
    let detail = runtime.get_run(&root_id).await.unwrap().unwrap();
    assert_eq!(detail.title, summary.title);
    assert_eq!(detail.task, task);
    assert_eq!(detail.reply, Some(reply));
    assert_eq!(detail.platform, "telegram");
    assert_eq!(detail.bot_id, 11);
    for limit in [0, 101, u64::MAX] {
        assert!(runtime.list_runs(None, limit).await.is_err());
    }
    assert!(runtime.list_runs(Some("not-a-status"), 50).await.is_err());
}

#[tokio::test]
async fn child_run_detail_returns_original_task_and_captured_title() {
    let h = Harness::new(vec![
        step("", vec![delegate("list-child", json!([
            {"id":"research", "title":"Research source", "task":"Read the complete source\nKeep this second line"}
        ]))]),
        step("parent complete", vec![]),
    ]).await;
    h.route(
        "Read the complete source\nKeep this second line",
        vec![step("child complete", vec![])],
    );
    let runtime = h.runtime();
    let reply = runtime
        .submit_text(context("parent task"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let parent = runtime.get_run(&reply.run_id).await.unwrap().unwrap();
    let child = runtime
        .get_run(&parent.children[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.title, "Research source");
    assert_eq!(
        child.task,
        "Read the complete source\nKeep this second line"
    );
    assert_eq!(child.platform, parent.platform);
    assert_eq!(child.bot_id, parent.bot_id);
    assert_eq!(child.node_id.as_deref(), Some("research"));
    assert_eq!(child.parent_id.as_deref(), Some(reply.run_id.as_str()));
    assert_eq!(runtime.list_runs(None, 50).await.unwrap().len(), 1);
}

#[tokio::test]
async fn exact_approval_lookup_finds_paused_tasks_outside_latest_list_page() {
    use nuono_app::agent::store::AgentToolApprovalRecord;
    let h = Harness::new(vec![step("", vec![call("old-pending", "change")])]).await;
    let runtime = h.runtime();
    let reply = runtime
        .submit_text(context("old pending task"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let run = runtime.get_run(&reply.run_id).await.unwrap().unwrap();
    let approval_id = run.approval_id.unwrap();
    for index in 0..3 {
        h.db.create_agent_tool_approval(AgentToolApprovalRecord {
            run_id: None,
            context_json: None,
            session_id: format!("newer-approval-{index}"),
            bot_id: 1,
            platform: "onebot".into(),
            message_type: "private".into(),
            user_id: 2,
            group_id: None,
            channel_id: None,
            message_id: None,
            tool_name: "change".into(),
            risk_level: "requires_approval".into(),
            input_json: Some(json!({"index":index}).to_string()),
            reason: Some("newer pending approval".into()),
        })
        .await
        .unwrap();
    }
    let page =
        h.db.list_agent_tool_approvals(Some("pending"), 1)
            .await
            .unwrap();
    assert_eq!(page.len(), 1);
    assert_ne!(page[0].id, approval_id);
    let approval =
        h.db.get_agent_tool_approval(approval_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(approval.run_id.as_deref(), Some(reply.run_id.as_str()));
    assert_eq!(approval.status, "pending");
    assert_eq!(approval.tool_name, "change");
    assert_eq!(
        serde_json::from_str::<Value>(approval.input_json.as_deref().unwrap()).unwrap(),
        json!({"key":"old-pending"})
    );
    assert!(
        h.db.get_agent_tool_approval(i64::MAX)
            .await
            .unwrap()
            .is_none()
    );
}

const PERSONA_A: &str = "11111111-1111-4111-8111-111111111111";
const PERSONA_B: &str = "22222222-2222-4222-8222-222222222222";
fn persona_input(name: &str, prompt: &str) -> nuono_app::persona::PersonaInput {
    nuono_app::persona::PersonaInput {
        name: name.into(),
        system_prompt: prompt.into(),
        ..Default::default()
    }
}
fn persona_group(text: &str) -> AgentContext {
    let mut ctx = context(&format!("/ai {text}"));
    ctx.message_type = "group".into();
    ctx.group_id = Some(300);
    ctx
}
async fn bind_persona(h: &Harness, id: &str) {
    h.db.upsert_ai_persona_binding(nuono_app::persona::PersonaBinding {
        platform: "onebot".into(),
        bot_id: "1".into(),
        group_id: "300".into(),
        namespace: None,
        persona_id: id.into(),
    })
    .await
    .unwrap();
}
fn has_prompt(request: &ChatRequest, text: &str) -> bool {
    request
        .messages
        .iter()
        .any(|m| m.role == MessageRole::System && m.content.contains(text))
}

#[tokio::test]
async fn persona_switch_isolates_history_and_disabled_binding_uses_global_prompt() {
    let h = Harness::new(vec![
        step("A first reply", vec![]),
        step("A second reply", vec![]),
        step("B reply", vec![]),
        step("global reply", vec![]),
    ])
    .await;
    h.config.0.write().await.system_prompt = Some("宿主全局要求".into());
    h.config.0.write().await.agent_memory_enabled = true;
    h.db.create_ai_persona(PERSONA_A, persona_input("小菲比", "角色 A 特有性格"))
        .await
        .unwrap();
    h.db.create_ai_persona(PERSONA_B, persona_input("小露帕", "角色 B 特有性格"))
        .await
        .unwrap();
    bind_persona(&h, PERSONA_A).await;
    let first = h
        .runtime()
        .submit_text(persona_group("A first user"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    h.db.upsert_agent_memory(nuono_app::agent::store::AgentMemoryRecord {
        scope_type: "session".into(),
        scope_key: first.context.session_id(),
        memory_key: "character-history".into(),
        content: "A private session memory".into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        actor: None,
    })
    .await
    .unwrap();
    let second = h
        .runtime()
        .submit_text(persona_group("private session memory"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(first.context.session_id(), second.context.session_id());
    assert!(first.context.persona.is_some());
    bind_persona(&h, PERSONA_B).await;
    let third = h
        .runtime()
        .submit_text(persona_group("private session memory"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_ne!(third.context.session_id(), first.context.session_id());
    assert_eq!(
        third.context.base_session_id(),
        first.context.base_session_id()
    );
    let mut disabled = persona_input("小露帕", "角色 B 特有性格");
    disabled.enabled = false;
    h.db.update_ai_persona(PERSONA_B, 1, disabled)
        .await
        .unwrap();
    let fourth = h
        .runtime()
        .submit_text(persona_group("private session memory"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert!(fourth.context.persona.is_none());
    let requests = h.script.requests.lock().unwrap();
    assert!(!has_prompt(&requests[0], "宿主全局要求"));
    assert!(has_prompt(&requests[0], "角色 A 特有性格"));
    assert!(has_prompt(&requests[1], "A private session memory"));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains("A first reply"))
    );
    assert!(has_prompt(&requests[2], "角色 B 特有性格"));
    assert!(!has_prompt(&requests[2], "A private session memory"));
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|m| m.content.contains("A first reply") || m.content == "A first user")
    );
    assert!(has_prompt(&requests[3], "宿主全局要求"));
    assert!(!has_prompt(&requests[3], "角色 B 特有性格"));
    assert!(!has_prompt(&requests[3], "A private session memory"));
}

#[tokio::test]
async fn persona_approval_resume_keeps_old_snapshot_after_edit_and_rebinding() {
    let h = Harness::new(vec![
        step("", vec![call("write", "change")]),
        step("resumed as A", vec![]),
    ])
    .await;
    h.db.create_ai_persona(PERSONA_A, persona_input("小菲比", "A original prompt"))
        .await
        .unwrap();
    let audio_filename = format!("{PERSONA_B}.wav");
    h.db.set_persona_asset(
        PERSONA_A,
        nuono_app::persona::PersonaAssetKind::ReferenceAudio,
        Some(audio_filename.clone()),
        1,
    )
    .await
    .unwrap();
    h.db.create_ai_persona(PERSONA_B, persona_input("B", "B prompt"))
        .await
        .unwrap();
    bind_persona(&h, PERSONA_A).await;
    let paused = h
        .runtime()
        .submit_text(persona_group("work"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let run = h.paused().await;
    let old_session = paused.context.session_id();
    h.db.update_ai_persona(PERSONA_A, 2, persona_input("A edited", "A new prompt"))
        .await
        .unwrap();
    bind_persona(&h, PERSONA_B).await;
    let steered = h
        .runtime()
        .submit_text(persona_group("also explain"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(steered.run_id, run.id);
    let id = run.approval_id.unwrap();
    h.service().decide(&owner(), id, true, None).await.unwrap();
    let resumed = h.runtime().resume_approval(id).await.unwrap().unwrap();
    assert_eq!(resumed.context.session_id(), old_session);
    let persona = resumed.context.persona.unwrap();
    assert_eq!(persona.id, PERSONA_A);
    assert_eq!(persona.revision, 2);
    assert_eq!(persona.system_prompt, "A original prompt");
    assert_eq!(
        persona.reference_audio_filename.as_deref(),
        Some(audio_filename.as_str())
    );
    assert_eq!(
        persona
            .tts_request("新的回复")
            .reference_audio
            .unwrap()
            .filename,
        audio_filename
    );
    assert_eq!(
        h.contexts.lock().unwrap()[0]
            .persona
            .as_ref()
            .unwrap()
            .revision,
        2
    );
    let requests = h.script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(has_prompt(&requests[1], "A original prompt"));
    assert!(!has_prompt(&requests[1], "A new prompt"));
    assert!(!has_prompt(&requests[1], "B prompt"));
}

#[tokio::test]
async fn persona_switch_does_not_hide_an_active_run_from_cancellation() {
    let h = Harness::new(vec![step("", vec![call("write", "change")])]).await;
    h.db.create_ai_persona(PERSONA_A, persona_input("A", "A prompt"))
        .await
        .unwrap();
    h.db.create_ai_persona(PERSONA_B, persona_input("B", "B prompt"))
        .await
        .unwrap();
    bind_persona(&h, PERSONA_A).await;
    let paused = h
        .runtime()
        .submit_text(persona_group("work"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    bind_persona(&h, PERSONA_B).await;
    let cancelled = h
        .runtime()
        .submit_text(persona_group("取消任务"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(cancelled.run_id, paused.run_id);
    assert_eq!(
        h.runtime()
            .get_run(&paused.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert_eq!(h.script.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn persona_children_inherit_parent_revision_after_binding_changes_during_inference() {
    let gate = Arc::new(Gate::default());
    let mut fork = step(
        "",
        vec![delegate(
            "fork",
            json!([{"task":"character child","title":"child"}]),
        )],
    );
    fork.gate = Some(gate.clone());
    let h = Harness::new(vec![fork, step("parent done", vec![])]).await;
    h.route("character child", vec![step("child done", vec![])]);
    h.db.create_ai_persona(PERSONA_A, persona_input("A", "A original character"))
        .await
        .unwrap();
    h.db.create_ai_persona(PERSONA_B, persona_input("B", "B character"))
        .await
        .unwrap();
    bind_persona(&h, PERSONA_A).await;
    let task = tokio::spawn(
        h.runtime()
            .submit_text(persona_group("work"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&gate).await;
    h.db.update_ai_persona(
        PERSONA_A,
        1,
        persona_input("A edited", "A edited character"),
    )
    .await
    .unwrap();
    bind_persona(&h, PERSONA_B).await;
    gate.released.notify_one();
    let reply = task.await.unwrap().unwrap();
    assert_eq!(reply.context.persona.as_ref().unwrap().revision, 1);
    let children = h.db.list_agent_children(&reply.run_id).await.unwrap();
    assert_eq!(children.len(), 1);
    let saved: Value = serde_json::from_str(&children[0].state_json).unwrap();
    assert_eq!(saved["context"]["persona"]["id"], PERSONA_A);
    assert_eq!(saved["context"]["persona"]["revision"], "1");
    let requests = h.script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|r| has_prompt(r, "A original character"))
    );
    assert!(
        requests
            .iter()
            .all(|r| !has_prompt(r, "B character") && !has_prompt(r, "A edited character"))
    );
}

#[tokio::test]
async fn assigned_models_drive_advanced_and_tool_rounds_and_inherit_when_empty() {
    for dedicated_utility in [true, false] {
        let h = Harness::new(vec![
            step("", vec![call("lookup-1", "lookup")]),
            step("done", vec![]),
        ])
        .await;
        let advanced =
            h.db.create_ai_model(nuono_core::db::CreateAiModel {
                name: "Advanced",
                provider: "fixture",
                provider_type: "responses",
                model_id: "advanced-model",
                api_key: Some("fixture"),
                base_url: "https://example.invalid/v1",
                is_default: true,
            })
            .await
            .unwrap();
        let utility =
            h.db.create_ai_model(nuono_core::db::CreateAiModel {
                name: "Utility",
                provider: "fixture",
                provider_type: "openai_compatible",
                model_id: "utility-model",
                api_key: Some("fixture"),
                base_url: "https://example.invalid/v1",
                is_default: false,
            })
            .await
            .unwrap();
        {
            let mut config = h.config.0.write().await;
            config.provider_router_enabled = true;
            config.chat_model_alias = format!("id:{}", advanced.id);
            config.tool_model_alias = if dedicated_utility {
                format!("id:{}", utility.id)
            } else {
                String::new()
            };
        }
        let reply = h
            .runtime()
            .submit_text(context("look this up"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap();
        assert_eq!(reply.content, "done");
        let models = h
            .script
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.model.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            models,
            vec![
                "advanced-model",
                if dedicated_utility {
                    "utility-model"
                } else {
                    "advanced-model"
                }
            ]
        );
        assert_eq!(h.reads.load(Ordering::SeqCst), 1);
        h.db.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn group_members_share_context_and_quotes_without_triggering_on_ambient_messages() {
    let h = Harness::new(vec![
        step("group answer", vec![]),
        step("other group", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    let mut alice = persona_group("unused");
    alice.user_id = 101;
    alice.message_id = Some("alice-message".into());
    alice.content = "我选蓝色方案".into();
    alice.raw_message = alice.content.clone();
    alice.images = vec!["https://example.com/blue.png".into()];
    assert!(runtime.submit_text(alice.clone()).await.unwrap().is_none());
    assert!(h.script.requests.lock().unwrap().is_empty());

    let mut bob = persona_group("对比一下她的方案");
    bob.user_id = 202;
    bob.message_id = Some("bob-message".into());
    bob.reply_to_message_id = alice.message_id.clone();
    assert_eq!(alice.session_id(), bob.session_id());
    assert_ne!(alice.base_session_id(), bob.base_session_id());
    let reply = runtime
        .submit_text(bob.clone())
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(reply.content, "group answer");
    let mut elsewhere = bob;
    elsewhere.group_id = Some(301);
    runtime
        .submit_text(elsewhere)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let requests = h.script.requests.lock().unwrap();
    let current = requests[0].messages.last().unwrap();
    assert!(current.content.contains("bob-message"));
    assert!(current.content.contains("alice-message"));
    assert!(current.content.contains("我选蓝色方案"));
    assert!(current.content.contains("202"));
    assert!(current.content.contains("101"));
    assert_eq!(current.images, ["https://example.com/blue.png"]);
    assert!(
        requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("无法读取")
    );
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains("我选蓝色方案"))
    );
}

#[tokio::test]
async fn same_group_members_keep_independent_active_tasks() {
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    let h = Harness::new(vec![gated_step("A answer", &a), gated_step("B answer", &b)]).await;
    let runtime = h.runtime();
    let mut alice = persona_group("A task");
    alice.user_id = 101;
    let mut bob = persona_group("B task");
    bob.user_id = 202;
    let first = tokio::spawn(runtime.submit_text(alice.clone()).await.unwrap().unwrap());
    wait_for_model(&a).await;
    let second = tokio::spawn(runtime.submit_text(bob).await.unwrap().unwrap());
    wait_for_model(&b).await;
    alice.content = "/ai cancel".into();
    alice.raw_message = alice.content.clone();
    runtime
        .submit_text(alice)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(first.await.unwrap().unwrap().content, "任务已取消");
    b.released.notify_one();
    assert_eq!(second.await.unwrap().unwrap().content, "B answer");
}

#[tokio::test]
async fn image_steering_survives_the_next_model_turn() {
    let gate = Arc::new(Gate::default());
    let h = Harness::new(vec![
        gated_step("first", &gate),
        step("looked at image", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    let task = tokio::spawn(
        runtime
            .submit_text(context("look at the next image"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&gate).await;
    let mut image = context("and this one");
    image.images = vec!["https://example.com/next.png".into()];
    runtime
        .submit_text(image)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    gate.released.notify_one();
    assert_eq!(task.await.unwrap().unwrap().content, "looked at image");
    let requests = h.script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last().unwrap().images,
        ["https://example.com/next.png"]
    );
}

#[tokio::test]
async fn disabling_memory_during_inference_discards_the_old_memory_response() {
    let gate = Arc::new(Gate::default());
    let h = Harness::new(vec![
        gated_step("old memory answer", &gate),
        step("fresh answer", vec![]),
    ])
    .await;
    h.config.0.write().await.agent_memory_enabled = true;
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&gate).await;
    h.config.0.write().await.agent_memory_enabled = false;
    gate.released.notify_one();
    let reply = task.await.unwrap().unwrap();
    assert!(reply.content.contains("长期记忆已关闭"));
    assert_eq!(
        runtime
            .get_run(&reply.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    let next = runtime
        .submit_text(context("new task"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(next.content, "fresh answer");
    assert!(
        !h.script.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|m| m.content.contains("old memory answer"))
    );
}

#[tokio::test]
async fn reversed_group_answers_retain_their_request_member_and_message() {
    let a = Arc::new(Gate::default());
    let b = Arc::new(Gate::default());
    let h = Harness::new(vec![
        gated_step("A answer", &a),
        gated_step("B answer", &b),
        step("C answer", vec![]),
    ])
    .await;
    let runtime = h.runtime();
    let mut alice = persona_group("A question");
    alice.user_id = 101;
    alice.message_id = Some("request-A".into());
    let mut bob = persona_group("B question");
    bob.user_id = 202;
    bob.message_id = Some("request-B".into());
    let first = tokio::spawn(runtime.submit_text(alice).await.unwrap().unwrap());
    wait_for_model(&a).await;
    let second = tokio::spawn(runtime.submit_text(bob).await.unwrap().unwrap());
    wait_for_model(&b).await;
    b.released.notify_one();
    assert_eq!(second.await.unwrap().unwrap().content, "B answer");
    a.released.notify_one();
    assert_eq!(first.await.unwrap().unwrap().content, "A answer");
    let mut charlie = persona_group("谁的方案更合适");
    charlie.user_id = 303;
    runtime
        .submit_text(charlie)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let requests = h.script.requests.lock().unwrap();
    let answers = requests[2]
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 2);
    assert!(answers[0].content.contains("B answer"));
    assert!(answers[0].content.contains("request-B"));
    assert!(answers[0].content.contains("202"));
    assert!(answers[1].content.contains("A answer"));
    assert!(answers[1].content.contains("request-A"));
    assert!(answers[1].content.contains("101"));
}

async fn recovery_wait_for_idle(runtime: &AgentRuntime) {
    tokio::time::timeout(Duration::from_secs(10), runtime.wait_for_idle())
        .await
        .expect("Agent supervision did not finish cleanup")
        .expect("Agent cleanup failed");
}

async fn recovery_assert_interrupted(h: &Harness, id: &str) {
    let run = h.db.get_agent_run(id).await.unwrap().unwrap();
    assert_eq!(run.status, "interrupted");
    let checkpoint: Value = serde_json::from_str(&run.state_json).unwrap();
    assert!(checkpoint.get("interruption").is_some());
    assert!(
        checkpoint["reply"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty())
    );
    assert!(
        h.db.active_agent_run(&run.session_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn recovery_history_failure_after_create_does_not_keep_the_session_active() {
    let h = Harness::new(vec![step("new task accepted", vec![])]).await;
    let runtime = h.runtime();
    // 故障注入仅作用于 Harness::new 创建的独立测试数据库，不会影响其他测试。
    h.db.orm()
        .execute_unprepared("ALTER TABLE nuono_messages RENAME TO recovery_saved_messages")
        .await
        .unwrap();
    let submitted = runtime.submit_text(context("history failure")).await;
    assert!(submitted.is_err());
    recovery_wait_for_idle(&runtime).await;
    let failed = h.root().await;
    recovery_assert_interrupted(&h, &failed.id).await;
    assert!(h.script.requests.lock().unwrap().is_empty());
    h.db.orm()
        .execute_unprepared("ALTER TABLE recovery_saved_messages RENAME TO nuono_messages")
        .await
        .unwrap();
    let reply = runtime
        .submit_text(context("retry as a new task"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(reply.content, "new task accepted");
    assert_ne!(reply.run_id, failed.id);
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_account_blocked_mid_run_rejects_before_tools_and_frees_the_session() {
    let gate = Arc::new(Gate::default());
    let mut blocked = step("", vec![call("must-not-execute", "lookup")]);
    blocked.gate = Some(gate.clone());
    let h = Harness::new(vec![
        blocked,
        step("new request", vec![]),
        step("ok", vec![]),
    ])
    .await;
    let account_id =
        h.db.create_user(nuono_core::db::NewUser {
            username: "blocked-mid-run".into(),
            email: None,
            password: "Blocked-account-fixture-123!".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .unwrap()
        .id;
    let runtime = h.runtime();
    let task = tokio::spawn(runtime.submit_text(context("work")).await.unwrap().unwrap());
    wait_for_model(&gate).await;
    let run_row = h.root().await;
    h.config
        .0
        .write()
        .await
        .agent_denied_platforms
        .push("onebot".into());
    gate.released.notify_one();
    let reply = task.await.unwrap().unwrap();
    assert_eq!(reply.content, "当前配置已停止该任务");
    recovery_wait_for_idle(&runtime).await;
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    let saved = h.db.get_agent_run(&run_row.id).await.unwrap().unwrap();
    assert_eq!(saved.status, "rejected");
    h.config.0.write().await.agent_denied_platforms.clear();
    assert_eq!(
        runtime
            .submit_text(context("new work"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "new request"
    );
    assert_eq!(
        h.reads.load(Ordering::SeqCst),
        0,
        "rejected tool calls must never be replayed"
    );
    let account_ctx = |input: &str| {
        AgentContext::for_account_chat("fixture", account_id, input.into(), "负责个人对话".into())
    };
    h.config.0.write().await.blocked_accounts.push(account_id);
    assert!(
        runtime
            .submit_text(account_ctx("still blocked"))
            .await
            .unwrap()
            .is_none()
    );
    h.config.0.write().await.blocked_accounts.clear();
    assert_eq!(
        runtime
            .submit_text(account_ctx("resumed"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "ok"
    );
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_model_panic_is_contained_and_the_next_request_can_execute() {
    let mut panicking = step("never returned", vec![]);
    panicking.panic_after_start = true;
    let h = Harness::new(vec![panicking, step("after panic", vec![])]).await;
    let runtime = h.runtime();
    let task = runtime
        .submit_text(context("panic"))
        .await
        .unwrap()
        .unwrap();
    let failed = h.root().await;
    assert!(
        task.await.is_err(),
        "provider panic must become a task error"
    );
    recovery_wait_for_idle(&runtime).await;
    recovery_assert_interrupted(&h, &failed.id).await;
    assert_eq!(
        runtime
            .submit_text(context("new work"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "after panic"
    );
    assert_eq!(h.script.requests.lock().unwrap().len(), 2);
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_dropping_unpolled_work_marks_it_interrupted_without_model_calls() {
    let h = Harness::new(vec![step("only the replacement runs", vec![])]).await;
    let runtime = h.runtime();
    let task = runtime
        .submit_text(context("never polled"))
        .await
        .unwrap()
        .unwrap();
    let dropped = h.root().await;
    assert!(h.script.requests.lock().unwrap().is_empty());
    drop(task);
    recovery_wait_for_idle(&runtime).await;
    recovery_assert_interrupted(&h, &dropped.id).await;
    assert!(h.script.requests.lock().unwrap().is_empty());
    assert_eq!(
        runtime
            .submit_text(context("replacement"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "only the replacement runs"
    );
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_aborting_a_polled_task_cancels_the_model_and_prevents_later_tools() {
    let gate = Arc::new(Gate::default());
    let mut blocked = step("", vec![call("after-abort", "lookup")]);
    blocked.gate = Some(gate.clone());
    let h = Harness::new(vec![blocked, step("replacement", vec![])]).await;
    let runtime = h.runtime();
    let task = tokio::spawn(
        runtime
            .submit_text(context("aborted"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&gate).await;
    let aborted = h.root().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    recovery_wait_for_idle(&runtime).await;
    recovery_assert_interrupted(&h, &aborted.id).await;
    gate.released.notify_one();
    assert_eq!(
        runtime
            .submit_text(context("new work"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "replacement"
    );
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_shutdown_interrupts_polled_and_unpolled_work_and_closes_admission() {
    let gate = Arc::new(Gate::default());
    let mut blocked = step("", vec![call("after-shutdown", "lookup")]);
    blocked.gate = Some(gate.clone());
    let h = Harness::new(vec![blocked]).await;
    let runtime = h.runtime();
    let active = tokio::spawn(
        runtime
            .submit_text(context("active"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&gate).await;
    let mut other = context("prepared but unpolled");
    other.user_id = 3;
    let unpolled = runtime.submit_text(other).await.unwrap().unwrap();
    let rows = h.runs().await;
    assert_eq!(rows.len(), 2);
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    let _ = active.await.unwrap();
    drop(unpolled);
    recovery_wait_for_idle(&runtime).await;
    for row in rows {
        recovery_assert_interrupted(&h, &row.id).await;
    }
    gate.released.notify_one();
    assert!(runtime.submit_text(context("after stop")).await.is_err());
    assert!(runtime.resume_approval(-1).await.is_err());
    assert!(runtime.collect_children("missing").await.is_err());
    assert_eq!(h.script.requests.lock().unwrap().len(), 1);
    assert_eq!(h.reads.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_shutdown_drops_an_inflight_tool_before_it_can_finish() {
    let gate = Arc::new(Gate::default());
    let probe = Arc::new(ToolProbe::default());
    let h = Harness::new(vec![
        step("", vec![call("stopped-tool", "lookup")]),
        step("replacement", vec![]),
    ])
    .await;
    register_coordinated(
        &h,
        "lookup",
        ToolRiskLevel::ReadOnly,
        ToolScheduling::Exclusive,
        &[("stopped-tool", gate.clone())],
        &probe,
    );
    let runtime = h.runtime();
    let task = tokio::spawn(
        runtime
            .submit_text(context("tool work"))
            .await
            .unwrap()
            .unwrap(),
    );
    wait_for_model(&gate).await;
    let interrupted = h.root().await;
    assert_eq!(probe.active.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    let _ = task.await.unwrap();
    recovery_assert_interrupted(&h, &interrupted.id).await;
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    assert_eq!(*probe.events.lock().unwrap(), ["start:stopped-tool"]);
    gate.released.notify_one();
    let replacement = h.runtime();
    assert_eq!(
        replacement
            .submit_text(context("new task"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "replacement"
    );
    assert_eq!(*probe.events.lock().unwrap(), ["start:stopped-tool"]);
    replacement.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_restart_interrupts_running_work_and_releases_its_conversation() {
    let h = Harness::new(vec![
        step("", vec![call("uncertain", "change")]),
        step("fresh start", vec![]),
    ])
    .await;
    let old_runtime = h.runtime();
    old_runtime
        .submit_text(context("old task"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let paused = h.paused().await;
    old_runtime.shutdown().await.unwrap();
    // 模拟旧进程在写入暂停检查点之前意外退出的残留状态。
    let mut running: nuono_app::agent::store::AgentRunRecord = paused.clone().into();
    let revision = running.revision;
    running.status = "running".into();
    running.approval_id = None;
    assert!(h.db.update_agent_run(running, revision).await.unwrap());
    h.db.reconcile_agent_runs_before_start("test process restarted")
        .await
        .unwrap();
    recovery_assert_interrupted(&h, &paused.id).await;
    h.db.decide_agent_tool_approval(
        paused.approval_id.unwrap(),
        true,
        Some("owner".into()),
        None,
    )
    .await
    .unwrap();
    assert!(
        h.db.claim_agent_tool_approval_for_execution(paused.approval_id.unwrap())
            .await
            .unwrap()
            .is_none()
    );
    let new_runtime = h.runtime();
    assert_eq!(
        new_runtime
            .submit_text(context("new task"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "fresh start"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 0);
    assert_eq!(h.script.requests.lock().unwrap().len(), 2);
    new_runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_restart_preserves_approved_checkpoint_and_never_replays_saved_effects() {
    let h = Harness::new(vec![
        step("", vec![call("write", "change")]),
        step("saved result consumed", vec![]),
    ])
    .await;
    let old_runtime = h.runtime();
    old_runtime
        .submit_text(context("work"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let paused = h.paused().await;
    let approval = paused.approval_id.unwrap();
    h.service()
        .decide(&owner(), approval, true, None)
        .await
        .unwrap();
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    let saved =
        h.db.get_agent_tool_approval(approval)
            .await
            .unwrap()
            .unwrap()
            .execution_json;
    assert!(saved.is_some());
    old_runtime.shutdown().await.unwrap();
    h.db.reconcile_agent_runs_before_start("test process restarted")
        .await
        .unwrap();
    let recovered = h.db.get_agent_run(&paused.id).await.unwrap().unwrap();
    assert_eq!(recovered.status, "waiting_approval");
    assert_eq!(recovered.revision, paused.revision);
    assert_eq!(
        h.db.get_agent_tool_approval(approval)
            .await
            .unwrap()
            .unwrap()
            .execution_json,
        saved
    );
    let new_runtime = h.runtime();
    assert_eq!(
        new_runtime
            .resume_approval(approval)
            .await
            .unwrap()
            .unwrap()
            .content,
        "saved result consumed"
    );
    assert!(
        new_runtime
            .resume_approval(approval)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    new_runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_restart_closes_uncertain_child_graph_without_replaying_any_branch() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([
                    {"id":"source", "task":"uncertain-source", "tools":["change"]},
                    {"id":"next", "task":"unstarted-successor", "depends_on":["source"]}
                ]),
            )],
        ),
        step("new task after graph interruption", vec![]),
    ])
    .await;
    h.route(
        "uncertain-source",
        vec![step("", vec![call("write", "change")])],
    );
    h.route("unstarted-successor", vec![]);
    let old_runtime = h.runtime();
    old_runtime
        .submit_text(context("graph"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let root = h.root().await;
    assert_eq!(root.status, "waiting_children");
    let children = h.db.list_agent_children(&root.id).await.unwrap();
    let source = children
        .iter()
        .find(|row| row.status == "waiting_approval")
        .unwrap();
    let approval = source.approval_id.unwrap();
    // 先持久化已执行的工具副作用，再模拟子任务恢复执行结果不确定的状态。
    h.service()
        .decide(&owner(), approval, true, None)
        .await
        .unwrap();
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    old_runtime.shutdown().await.unwrap();
    let mut uncertain = source.clone();
    let revision = uncertain.revision;
    uncertain.status = "running".into();
    uncertain.approval_id = None;
    assert!(h.db.update_agent_run(uncertain, revision).await.unwrap());
    h.db.reconcile_agent_runs_before_start("test child owner disappeared")
        .await
        .unwrap();
    recovery_assert_interrupted(&h, &root.id).await;
    for child in children {
        recovery_assert_interrupted(&h, &child.id).await;
    }
    let new_runtime = h.runtime();
    assert!(
        new_runtime
            .collect_children(&root.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        new_runtime
            .resume_approval(approval)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    assert!(task_requests(&h, "unstarted-successor").is_empty());
    assert_eq!(
        new_runtime
            .submit_text(context("fresh root"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "new task after graph interruption"
    );
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    new_runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_restart_keeps_healthy_child_approval_graph_resumable() {
    let h = Harness::new(vec![
        step(
            "",
            vec![delegate(
                "fork",
                json!([{"task":"healthy-child", "tools":["change"]}]),
            )],
        ),
        step("parent done", vec![]),
    ])
    .await;
    h.route(
        "healthy-child",
        vec![
            step("", vec![call("write", "change")]),
            step("child done", vec![]),
        ],
    );
    let old_runtime = h.runtime();
    old_runtime
        .submit_text(context("graph"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let root = h.root().await;
    let child = h.paused().await;
    old_runtime.shutdown().await.unwrap();
    h.db.reconcile_agent_runs_before_start("test healthy pause recovery")
        .await
        .unwrap();
    assert_eq!(
        h.db.get_agent_run(&root.id).await.unwrap().unwrap().status,
        "waiting_children"
    );
    assert_eq!(
        h.db.get_agent_run(&child.id).await.unwrap().unwrap().status,
        "waiting_approval"
    );
    let approval = child.approval_id.unwrap();
    h.service()
        .decide(&owner(), approval, true, None)
        .await
        .unwrap();
    let new_runtime = h.runtime();
    let reply = new_runtime
        .resume_approval(approval)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.run_id, root.id);
    assert_eq!(reply.content, "parent done");
    assert_eq!(h.effects.load(Ordering::SeqCst), 1);
    new_runtime.shutdown().await.unwrap();
    h.db.shutdown().await.unwrap();
}
