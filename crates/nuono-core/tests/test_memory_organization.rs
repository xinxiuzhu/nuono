//! 验证 PostgreSQL 与 SQLite 下的记忆遗忘、作用域隔离、空闲整理准入以及证据图谱持久化。
use async_trait::async_trait;
use chrono::{Duration, Utc};
use nuono_app::{
    agent::{
        AgentContext,
        memory::*,
        ports::{AgentConfiguration, ModelProviderFactory},
        runtime::AgentRuntime,
        store::{AgentMemoryRecord, AgentMemoryUpdateRecord, MemoryEntry},
        tool::BuiltinToolRegistry,
    },
    ai::{AiConfig, AiError, AiService, ChatRequest, ChatResponse, ChatStreamChunk, UsageInfo},
    settings::AIConfig,
};
use nuono_core::{db::Database, test_support};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, RwLock};

struct Config(RwLock<AIConfig>);
#[async_trait]
impl AgentConfiguration for Config {
    async fn ai_settings(&self) -> AIConfig {
        self.0.read().await.clone()
    }
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    released: Notify,
}
struct Model {
    requests: Mutex<Vec<ChatRequest>>,
    gate: Mutex<Option<Arc<Gate>>>,
    foreign_id: AtomicUsize,
}
#[async_trait]
impl AiService for Model {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AiError> {
        self.requests.lock().unwrap().push(request.clone());
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.released.notified().await;
        }
        let source: Vec<MemoryEntry> =
            serde_json::from_str(&request.messages.last().unwrap().content).unwrap();
        let mut ids: Vec<_> = source.iter().map(|m| m.id).collect();
        let foreign = self.foreign_id.load(Ordering::SeqCst);
        if foreign > 0 {
            ids.push(i64::try_from(foreign).unwrap());
        }
        Ok(ChatResponse { id:"fixture".into(),model:"utility".into(),content:json!({"topics":[{"name":"开发偏好","summary":"成员的偏好发生变化，保留原始依据","source_ids":ids}],"links":[{"from_id":source[0].id,"to_id":source[1].id,"relation":"conflicts","reason":"原始偏好不一致"}]}).to_string(),usage:UsageInfo {prompt_tokens:1,completion_tokens:1,total_tokens:2},finish_reason:Some("stop".into()),tool_calls:vec![],continuation:vec![] })
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChatStreamChunk, AiError>>, AiError> {
        Err(AiError::Other("fixture".into()))
    }
    fn service_name(&self) -> &str {
        "fixture"
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
    runtime: AgentRuntime,
}
impl Harness {
    async fn new() -> Self {
        let db = test_support::test_db(3).await;
        let config = Arc::new(Config(RwLock::new(AIConfig {
            enabled: true,
            reply_enabled: true,
            agent_memory_enabled: true,
            provider_router_enabled: false,
            provider_health_fuse_enabled: false,
            provider: "fixture".into(),
            model: "advanced".into(),
            api_key: Some("fixture".into()),
            agent_memory_policy: MemoryPolicy {
                organization_enabled: true,
                forgetting_enabled: true,
                ..Default::default()
            },
            ..Default::default()
        })));
        let model = Arc::new(Model {
            requests: Mutex::new(vec![]),
            gate: Mutex::new(None),
            foreign_id: AtomicUsize::new(0),
        });
        let runtime = AgentRuntime::with_ports(
            Arc::new(db.clone()),
            config.clone(),
            Arc::new(Factory(model.clone())),
            BuiltinToolRegistry::reply_only(),
        );
        Self {
            db,
            config,
            model,
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
    fn scope(key: &str) -> MemoryScope {
        MemoryScope {
            scope_type: "user".into(),
            scope_key: key.into(),
        }
    }
}
fn context() -> AgentContext {
    AgentContext {
        account_chat: None,
        persona: None,
        native: None,
        bot_id: 1,
        platform: "onebot".into(),
        message_type: "group".into(),
        user_id: 1,
        group_id: Some(1),
        channel_id: None,
        message_id: None,
        raw_message: "ambient activity".into(),
        content: "ambient activity".into(),
        images: vec![],
        reply_to_message_id: None,
    }
}

#[tokio::test]
async fn organization_uses_utility_and_isolates_scopes_without_rewriting_sources() {
    let h = Harness::new().await;
    let first = h.seed("a", "preference", "以前喜欢 Rust").await;
    h.seed("a", "new-preference", "现在喜欢 Python").await;
    h.seed("b", "secret", "foreign-scope-sentinel").await;
    let utility =
        h.db.create_ai_model(nuono_core::db::CreateAiModel {
            name: "Light",
            provider: "fixture",
            provider_type: "responses",
            model_id: "light-model",
            api_key: Some("fixture"),
            base_url: "https://example.invalid/v1",
            is_default: true,
        })
        .await
        .unwrap();
    {
        let mut c = h.config.0.write().await;
        c.provider_router_enabled = true;
        c.tool_model_alias = format!("id:{}", utility.id);
    }
    assert_eq!(
        h.runtime.maintain_memories(true).await.unwrap().status,
        "active"
    );
    assert!(h.model.requests.lock().unwrap().is_empty());
    let conversation = h.runtime.begin_conversation();
    assert_eq!(
        h.runtime.maintain_memories(false).await.unwrap().status,
        "active"
    );
    assert!(h.model.requests.lock().unwrap().is_empty());
    drop(conversation);
    let result = h.runtime.maintain_memories(false).await.unwrap();
    assert_eq!(result.status, "organized");
    let request = h.model.requests.lock().unwrap()[0].clone();
    assert_eq!(request.model.as_deref(), Some("light-model"));
    assert!(request.tools.is_empty());
    assert!(
        !request
            .messages
            .last()
            .unwrap()
            .content
            .contains("foreign-scope-sentinel")
    );
    let graph =
        h.db.memory_graph(&Harness::scope("a"))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(graph.graph.links[0].relation, "conflicts");
    assert_eq!(
        h.db.get_agent_memory(first.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        first.content
    );
    assert!(
        h.db.memory_graph(&Harness::scope("b"))
            .await
            .unwrap()
            .is_none()
    );
    // 轮转扫描到其他小规模作用域后，若数据未发生变化则跳过冗余的模型调用。
    assert_eq!(
        h.runtime.maintain_memories(false).await.unwrap().status,
        "too_few"
    );
    assert_eq!(
        h.runtime.maintain_memories(false).await.unwrap().status,
        "unchanged"
    );
    assert_eq!(h.model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn graph_rejects_foreign_ids_and_changed_sources() {
    let h = Harness::new().await;
    let a = h.seed("a", "one", "fact one").await;
    h.seed("a", "two", "fact two").await;
    let b = h.seed("b", "foreign", "foreign fact").await;
    h.model.foreign_id.store(b.id as usize, Ordering::SeqCst);
    assert!(h.runtime.maintain_memories(false).await.is_err());
    assert!(
        h.db.memory_graph(&Harness::scope("a"))
            .await
            .unwrap()
            .is_none()
    );
    let source =
        h.db.list_agent_memories(Some("user"), Some("a"), Some(true), true, 64)
            .await
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
    let graph = MemoryGraphRecord {
        scope: Harness::scope("a"),
        source_fingerprint: fingerprint(&source),
        source_limit: 64,
        graph: MemoryGraph {
            topics: vec![MemoryTopic {
                name: "topic".into(),
                summary: "summary".into(),
                source_ids: source.iter().map(|m| m.id).collect(),
            }],
            links: vec![],
        },
        updated_at: Utc::now(),
    };
    h.db.update_agent_memory(
        a.id,
        AgentMemoryUpdateRecord {
            content: Some("changed while organizing".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!h.db.save_memory_graph(graph).await.unwrap());
}

#[tokio::test]
async fn activity_and_disabling_memory_discard_pending_organization() {
    for disable in [false, true] {
        let h = Harness::new().await;
        h.seed("a", "one", "fact one").await;
        h.seed("a", "two", "fact two").await;
        let gate = Arc::new(Gate::default());
        *h.model.gate.lock().unwrap() = Some(gate.clone());
        let job = tokio::spawn({
            let runtime = h.runtime.clone();
            async move { runtime.maintain_memories(false).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            h.runtime.maintain_memories(false).await.unwrap().status,
            "busy"
        );
        if disable {
            h.config.0.write().await.agent_memory_enabled = false;
        } else {
            assert!(h.runtime.submit_text(context()).await.unwrap().is_none());
        }
        gate.released.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), job)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .status,
            "interrupted"
        );
        assert!(
            h.db.memory_graph(&Harness::scope("a"))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn forgetting_is_reversible_and_passive_reads_do_not_rehearse_every_memory() {
    let h = Harness::new().await;
    let old = h.seed("a", "old", "unrelated forgotten sentinel").await;
    let pinned = h.seed("a", "important", "always preserved sentinel").await;
    let old_at = Utc::now() - Duration::days(600);
    nuono_core::entity::agent_memories::Entity::update_many()
        .set(nuono_core::entity::agent_memories::ActiveModel {
            updated_at: Set(nuono_core::clock::store_time(old_at)),
            ..Default::default()
        })
        .filter(nuono_core::entity::agent_memories::Column::ScopeKey.eq("a"))
        .exec(h.db.orm())
        .await
        .unwrap();
    nuono_core::entity::agent_memories::Entity::update_many()
        .set(nuono_core::entity::agent_memories::ActiveModel {
            updated_at: Set(nuono_core::clock::store_time(old_at - Duration::days(100))),
            ..Default::default()
        })
        .filter(nuono_core::entity::agent_memories::Column::Id.eq(pinned.id))
        .exec(h.db.orm())
        .await
        .unwrap();
    h.db.pin_memory(pinned.id, true).await.unwrap();
    assert_eq!(
        h.db.recall_memory_candidates(&[("user".into(), "a".into())], 1)
            .await
            .unwrap()[0]
            .id,
        pinned.id,
        "an old pinned memory was crowded out by recency"
    );
    let policy = h.config.0.read().await.agent_memory_policy.clone();
    let scopes = vec![("user".into(), "a".into())];
    let first = recall(&h.db, &scopes, "unmatched request", &policy, 12)
        .await
        .unwrap();
    assert!(
        first.is_empty(),
        "pinning must not admit unrelated memories"
    );
    assert!(!first.iter().any(|m| m.id == old.id));
    assert!(h.db.get_agent_memory(old.id).await.unwrap().is_some());
    assert_eq!(
        h.db.memory_profiles(&[pinned.id]).await.unwrap()[0].recall_count,
        0
    );
    let revived = recall(&h.db, &scopes, "unrelated forgotten sentinel", &policy, 12)
        .await
        .unwrap();
    assert!(revived.iter().any(|m| m.id == old.id));
    let profile = h.db.memory_profiles(&[old.id]).await.unwrap().remove(0);
    assert_eq!(profile.recall_count, 1);
    assert!(profile.last_recalled_at.is_some());
    h.db.pin_memory(old.id, true).await.unwrap();
    h.db.delete_agent_memory(old.id).await.unwrap();
    assert!(h.db.memory_profiles(&[old.id]).await.unwrap().is_empty());
}

#[tokio::test]
async fn logical_relationships_only_reorder_relevant_hits_and_invalidate_after_source_edits() {
    let h = Harness::new().await;
    let rust = h.seed("a", "language", "Rust developer").await;
    let python = h.seed("a", "current", "Python developer").await;
    for n in 0..8 {
        h.seed("a", &format!("junk{n}"), &format!("ordinary filler {n}"))
            .await;
    }
    let source =
        h.db.list_agent_memories(Some("user"), Some("a"), Some(true), true, 64)
            .await
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<MemoryEntry>>();
    let graph = MemoryGraphRecord {
        scope: Harness::scope("a"),
        source_fingerprint: fingerprint(&source),
        source_limit: 64,
        updated_at: Utc::now(),
        graph: MemoryGraph {
            topics: vec![
                MemoryTopic {
                    name: "Rust and Python".into(),
                    summary: "Development languages".into(),
                    source_ids: vec![rust.id, python.id],
                },
                MemoryTopic {
                    name: "Other".into(),
                    summary: "Other notes".into(),
                    source_ids: source
                        .iter()
                        .map(|m| m.id)
                        .filter(|id| *id != rust.id && *id != python.id)
                        .collect(),
                },
            ],
            links: vec![MemoryLink {
                from_id: rust.id,
                to_id: python.id,
                relation: "updates".into(),
                reason: "Current language preference updates previous preference".into(),
            }],
        },
    };
    assert!(h.db.save_memory_graph(graph).await.unwrap());
    let scopes = vec![("user".into(), "a".into())];
    let policy = h.config.0.read().await.agent_memory_policy.clone();
    let recalled = recall(&h.db, &scopes, "Rust", &policy, 2).await.unwrap();
    assert!(recalled.iter().any(|m| m.id == rust.id));
    assert!(
        !recalled.iter().any(|m| m.id == python.id),
        "a link must not manufacture query relevance"
    );
    let both = recall(&h.db, &scopes, "Rust Python", &policy, 2)
        .await
        .unwrap();
    assert!(both.iter().any(|m| m.id == rust.id));
    assert!(both.iter().any(|m| m.id == python.id));
    h.db.update_agent_memory(
        rust.id,
        AgentMemoryUpdateRecord {
            content: Some("Rust corrected source".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        valid_graph(&h.db, &Harness::scope("a"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !recall(&h.db, &scopes, "Rust", &policy, 2)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == python.id)
    );
}
