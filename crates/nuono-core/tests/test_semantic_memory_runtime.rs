//! 使用固定解析向量对真实存储与运行时进行语义记忆回归测试（非模型效果评测）。
//! 测试中 "Cycling" 与 "commute" 固定共用向量 [1, 0]，无关测试数据使用 [0, 1]。
use async_trait::async_trait;
use chrono::{Duration, Utc};
use nuono_ai::embedding::{EmbeddingBatch, EmbeddingModel, EmbeddingPort};
use nuono_app::{
    agent::{
        AgentContext,
        memory::MemoryPolicy,
        ports::{AgentConfiguration, ModelProviderFactory},
        runtime::AgentRuntime,
        store::{AgentMemoryRecord, AgentMemoryUpdateRecord, MemoryEntry},
        tool::BuiltinToolRegistry,
    },
    ai::{
        AiConfig, AiError, AiService, ChatRequest, ChatResponse, ChatStreamChunk, MessageRole,
        UsageInfo,
    },
    settings::AIConfig,
};
use nuono_core::{db::Database, entity::agent_memory_embeddings, test_support};
use nuono_error::{NuonoError, Result};
use sea_orm::{EntityTrait, PaginatorTrait, Set};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, RwLock};

const SOURCE: &str = "Cycling is my regular way of travelling.";
const QUERY: &str = "commute";

struct Config(RwLock<AIConfig>);
#[async_trait]
impl AgentConfiguration for Config {
    async fn ai_settings(&self) -> AIConfig {
        self.0.read().await.clone()
    }
}

#[derive(Default)]
struct ChatRecorder(Mutex<Vec<ChatRequest>>);
#[async_trait]
impl AiService for ChatRecorder {
    async fn chat(&self, request: ChatRequest) -> std::result::Result<ChatResponse, AiError> {
        self.0.lock().unwrap().push(request);
        Ok(ChatResponse {
            id: "fixture-response".into(),
            model: "fixture-chat".into(),
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
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<ChatStreamChunk, AiError>>,
        AiError,
    > {
        Err(AiError::Other("fixture uses complete messages".into()))
    }
    fn service_name(&self) -> &str {
        "semantic-memory-fixture"
    }
}
struct Factory(Arc<ChatRecorder>);
impl ModelProviderFactory for Factory {
    fn create(&self, _: &AiConfig) -> std::result::Result<Arc<dyn AiService>, AiError> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

struct Embeddings {
    selected: RwLock<EmbeddingModel>,
    descriptions: AtomicUsize,
    inputs: Mutex<Vec<Vec<String>>>,
    unsupported: AtomicBool,
    fail_embed: AtomicBool,
    next_gate: Mutex<Option<Arc<Gate>>>,
}
impl Embeddings {
    fn new() -> Self {
        Self {
            selected: RwLock::new(EmbeddingModel {
                id: 7,
                model: "analytic-fixture".into(),
                revision: "revision-a".into(),
            }),
            descriptions: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
            unsupported: AtomicBool::new(false),
            fail_embed: AtomicBool::new(false),
            next_gate: Mutex::new(None),
        }
    }
    fn calls(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
    fn pause_next(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        *self.next_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}
#[async_trait]
impl EmbeddingPort for Embeddings {
    async fn describe(&self, model_id: i64) -> Result<EmbeddingModel> {
        self.descriptions.fetch_add(1, Ordering::SeqCst);
        if self.unsupported.load(Ordering::SeqCst) || model_id != 7 {
            return Err(NuonoError::Validation("fixture unsupported model".into()));
        }
        Ok(self.selected.read().await.clone())
    }
    async fn embed(&self, model_id: i64, input: Vec<String>) -> Result<EmbeddingBatch> {
        assert_eq!(model_id, 7);
        self.inputs.lock().unwrap().push(input.clone());
        let model = self.selected.read().await.clone();
        let gate = self.next_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        if self.fail_embed.load(Ordering::SeqCst) {
            return Err(NuonoError::Validation("fixture embedding failure".into()));
        }
        Ok(EmbeddingBatch {
            model,
            vectors: input
                .iter()
                .map(|text| {
                    if text.contains("Cycling") || text == QUERY {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect(),
        })
    }
}

struct Harness {
    db: Database,
    config: Arc<Config>,
    chat: Arc<ChatRecorder>,
    embeddings: Arc<Embeddings>,
    runtime: AgentRuntime,
}
impl Harness {
    async fn new(model_id: Option<i64>) -> Self {
        let db = test_support::test_db(3).await;
        let config = Arc::new(Config(RwLock::new(AIConfig {
            enabled: true,
            reply_enabled: true,
            provider: "fixture".into(),
            model: "fixture-chat".into(),
            api_key: Some("fixture".into()),
            provider_router_enabled: false,
            provider_health_fuse_enabled: false,
            agent_memory_enabled: true,
            agent_memory_auto_extract_enabled: false,
            agent_memory_policy: MemoryPolicy {
                embedding_model_id: model_id,
                ..Default::default()
            },
            ..Default::default()
        })));
        let chat = Arc::new(ChatRecorder::default());
        let embeddings = Arc::new(Embeddings::new());
        let runtime = AgentRuntime::with_ports(
            Arc::new(db.clone()),
            config.clone(),
            Arc::new(Factory(chat.clone())),
            BuiltinToolRegistry::reply_only(),
        )
        .with_memory_embeddings(embeddings.clone());
        Self {
            db,
            config,
            chat,
            embeddings,
            runtime,
        }
    }
    async fn seed(&self, scope: &str, key: &str, content: &str) -> MemoryEntry {
        self.db
            .upsert_agent_memory(AgentMemoryRecord {
                scope_type: "user".into(),
                scope_key: scope.into(),
                memory_key: key.into(),
                content: content.into(),
                tags_json: None,
                enabled: true,
                expires_at: None,
                actor: None,
            })
            .await
            .unwrap()
            .0
            .into()
    }
    async fn warm(&self, expected: usize) {
        let mut count = 0;
        for _ in 0..128 {
            count += self
                .runtime
                .maintain_semantic_memories(false)
                .await
                .unwrap();
            if count >= expected {
                break;
            }
        }
        assert_eq!(count, expected);
        assert_eq!(
            agent_memory_embeddings::Entity::find()
                .count(self.db.orm())
                .await
                .unwrap(),
            expected as u64
        );
    }
    async fn ask(&self, query: &str) -> ChatRequest {
        ask(self.runtime.clone(), query).await;
        self.last_request()
    }
    fn last_request(&self) -> ChatRequest {
        self.chat.0.lock().unwrap().last().unwrap().clone()
    }
}

fn context(query: &str) -> AgentContext {
    AgentContext {
        account_chat: None,
        persona: None,
        native: None,
        bot_id: 1,
        platform: "onebot".into(),
        message_type: "private".into(),
        user_id: 1,
        group_id: None,
        channel_id: None,
        message_id: None,
        raw_message: query.into(),
        content: query.into(),
        images: Vec::new(),
        reply_to_message_id: None,
    }
}
async fn ask(runtime: AgentRuntime, query: &str) {
    let response = runtime
        .submit_text(context(query))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(response.content, "收到");
}
fn memory_prompt(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| {
            message.role == MessageRole::System
                && message
                    .content
                    .starts_with("Persistent memory that may be relevant")
        })
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
async fn entered(gate: &Gate) {
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("fixture port was not entered");
}

#[tokio::test]
async fn default_lexical_memory_makes_no_embedding_requests() {
    let h = Harness::new(None).await;
    let fact = "ZX42 requires annual calibration.";
    h.seed("onebot:1", "equipment", fact).await;
    assert_eq!(
        h.runtime.maintain_semantic_memories(false).await.unwrap(),
        0
    );
    assert!(memory_prompt(&h.ask("ZX42").await).contains(fact));
    assert_eq!(h.embeddings.descriptions.load(Ordering::SeqCst), 0);
    assert_eq!(h.embeddings.calls(), 0);
    assert_eq!(
        agent_memory_embeddings::Entity::find()
            .count(h.db.orm())
            .await
            .unwrap(),
        0
    );
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn warmed_semantics_recalls_an_old_fact_beyond_the_recent_candidate_window() {
    use nuono_core::entity::agent_memories as memories;
    let h = Harness::new(Some(7)).await;
    h.config
        .0
        .write()
        .await
        .agent_memory_policy
        .forgetting_enabled = true;
    let source = h.seed("onebot:1", "transport", SOURCE).await;
    h.db.pin_memory(source.id, false).await.unwrap();
    let now = Utc::now();
    memories::Entity::insert_many((0..320).map(|number| memories::ActiveModel {
        scope_type: Set("user".into()),
        scope_key: Set("onebot:1".into()),
        memory_key: Set(format!("inventory-{number}")),
        content: Set(format!("Unrelated inventory item {number}")),
        tags_json: Set(None),
        enabled: Set(true),
        expires_at: Set(None),
        created_by: Set(None),
        updated_by: Set(None),
        created_at: Set(nuono_core::clock::store_time(
            now + Duration::seconds(number),
        )),
        updated_at: Set(nuono_core::clock::store_time(
            now + Duration::seconds(number),
        )),
        ..Default::default()
    }))
    .exec(h.db.orm())
    .await
    .unwrap();
    assert!(
        !h.db
            .recall_memory_candidates(&[("user".into(), "onebot:1".into())], 256)
            .await
            .unwrap()
            .iter()
            .any(|memory| memory.id == source.id)
    );
    // 前台冷启动召回不得同步触发全量语料的向量化计算。
    assert!(memory_prompt(&h.ask(QUERY).await).is_empty());
    assert_eq!(h.embeddings.calls(), 0);
    h.warm(321).await;
    let warm_calls = h.embeddings.calls();
    assert_eq!(warm_calls, 11);
    assert!(
        h.embeddings
            .inputs
            .lock()
            .unwrap()
            .iter()
            .all(|batch| batch.len() <= 32)
    );
    let prompt = memory_prompt(&h.ask(QUERY).await);
    assert!(prompt.contains(SOURCE), "prompt={prompt}");
    assert!(!prompt.contains("Unrelated inventory item"));
    assert_eq!(h.embeddings.calls(), warm_calls + 1);
    let profile = h.db.memory_profiles(&[source.id]).await.unwrap().remove(0);
    assert_eq!(
        profile.recall_count, 1,
        "one prompt inclusion reinforces exactly once"
    );
    assert!(profile.last_recalled_at.is_some());
    assert_eq!(
        h.embeddings.inputs.lock().unwrap().last().unwrap(),
        &[QUERY.to_string()]
    );
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn semantics_never_injects_foreign_disabled_or_expired_cached_facts() {
    let h = Harness::new(Some(7)).await;
    h.seed("onebot:1", "allowed", SOURCE).await;
    h.seed("onebot:2", "foreign", "Cycling ForeignScopeSentinel")
        .await;
    let disabled = h
        .seed("onebot:1", "disabled", "Cycling DisabledSentinel")
        .await;
    let expired = h
        .seed("onebot:1", "expired", "Cycling ExpiredSentinel")
        .await;
    h.warm(4).await;
    h.db.update_agent_memory(
        disabled.id,
        AgentMemoryUpdateRecord {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.db.update_agent_memory(
        expired.id,
        AgentMemoryUpdateRecord {
            expires_at: Some(Some(Utc::now() - Duration::seconds(1))),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let prompt = memory_prompt(&h.ask(QUERY).await);
    assert!(prompt.contains(SOURCE));
    for forbidden in [
        "ForeignScopeSentinel",
        "DisabledSentinel",
        "ExpiredSentinel",
    ] {
        assert!(!prompt.contains(forbidden), "prompt={prompt}");
    }
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_or_failed_embedding_models_keep_lexical_recall() {
    let h = Harness::new(Some(7)).await;
    let fact = "ZX42 requires annual calibration.";
    h.seed("onebot:1", "equipment", fact).await;
    h.embeddings.unsupported.store(true, Ordering::SeqCst);
    assert!(h.runtime.maintain_semantic_memories(false).await.is_err());
    assert!(memory_prompt(&h.ask("ZX42").await).contains(fact));
    assert_eq!(h.embeddings.calls(), 0);
    h.embeddings.unsupported.store(false, Ordering::SeqCst);
    h.warm(1).await;
    h.embeddings.fail_embed.store(true, Ordering::SeqCst);
    assert!(memory_prompt(&h.ask("ZX42").await).contains(fact));
    assert_eq!(h.embeddings.calls(), 2);
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_edit_during_query_embedding_drops_the_old_content_before_prompt_assembly() {
    let h = Harness::new(Some(7)).await;
    h.config
        .0
        .write()
        .await
        .agent_memory_policy
        .forgetting_enabled = true;
    let source = h.seed("onebot:1", "transport", SOURCE).await;
    h.db.pin_memory(source.id, false).await.unwrap();
    h.warm(1).await;
    let gate = h.embeddings.pause_next();
    let runtime = h.runtime.clone();
    let task = tokio::spawn(async move { ask(runtime, QUERY).await });
    entered(&gate).await;
    h.db.update_agent_memory(
        source.id,
        AgentMemoryUpdateRecord {
            content: Some("Now stays home; ChangedBodySentinel".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    gate.release.notify_one();
    task.await.unwrap();
    let prompt = memory_prompt(&h.last_request());
    assert!(!prompt.contains(SOURCE));
    assert!(!prompt.contains("ChangedBodySentinel"));
    let profile = h.db.memory_profiles(&[source.id]).await.unwrap().remove(0);
    assert_eq!(
        profile.recall_count, 0,
        "an edited, discarded hit must not be reinforced"
    );
    assert!(profile.last_recalled_at.is_none());
    let selected = h.embeddings.selected.read().await.clone();
    assert!(
        h.db.memory_embeddings(&[source.id], &selected)
            .await
            .unwrap()
            .is_empty()
    );
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_model_revision_requires_rewarming_and_does_not_reuse_old_vectors() {
    let h = Harness::new(Some(7)).await;
    let source = h.seed("onebot:1", "transport", SOURCE).await;
    h.warm(1).await;
    let previous = h.embeddings.selected.read().await.clone();
    h.embeddings.selected.write().await.revision = "revision-b".into();
    assert!(memory_prompt(&h.ask(QUERY).await).is_empty());
    assert_eq!(
        h.embeddings.calls(),
        1,
        "old-revision vectors must not cause a query embedding call"
    );
    h.warm(1).await;
    assert!(memory_prompt(&h.ask(QUERY).await).contains(SOURCE));
    assert_eq!(h.embeddings.calls(), 3);
    assert!(
        h.db.memory_embeddings(&[source.id], &previous)
            .await
            .unwrap()
            .is_empty()
    );
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_activity_cancels_idle_warming_without_publishing_the_cache() {
    let h = Harness::new(Some(7)).await;
    h.seed("onebot:1", "transport", SOURCE).await;
    let gate = h.embeddings.pause_next();
    let runtime = h.runtime.clone();
    let task = tokio::spawn(async move { runtime.maintain_semantic_memories(false).await });
    entered(&gate).await;
    h.runtime.notify_activity();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(
        agent_memory_embeddings::Entity::find()
            .count(h.db.orm())
            .await
            .unwrap(),
        0
    );
    h.warm(1).await;
    h.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn disabling_memory_during_query_embedding_prevents_prompt_injection() {
    let h = Harness::new(Some(7)).await;
    h.seed("onebot:1", "transport", SOURCE).await;
    h.warm(1).await;
    let gate = h.embeddings.pause_next();
    let runtime = h.runtime.clone();
    let task = tokio::spawn(async move {
        runtime
            .submit_text(context(QUERY))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
    });
    entered(&gate).await;
    h.config.0.write().await.agent_memory_enabled = false;
    gate.release.notify_one();
    let reply = task.await.unwrap();
    assert!(reply.content.contains("长期记忆已关闭"));
    assert!(h.chat.0.lock().unwrap().is_empty());
    let checkpoint = h.db.get_agent_run(&reply.run_id).await.unwrap().unwrap();
    assert!(!checkpoint.state_json.contains(SOURCE));
    h.db.shutdown().await.unwrap();
}
