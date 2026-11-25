//! 宿主到数据库的 AI 用量统计集成测试，所有推理请求均由本地回环测试服务响应。
use crate as nuono_core;
use nuono_core::{
    agent::{AgentContext, AgentRuntime, AgentTool, AgentToolContext, ToolRegistry},
    ai::{AiService, ChatMessage, ChatRequest},
    config::{AIConfig, ConfigManager, NuonoConfig},
    db::{CreateAiModel, Database},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

struct Fixture {
    base: String,
    tasks: JoinSet<Vec<Value>>,
}
impl Fixture {
    async fn new(responses: Vec<(u16, &'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), async move {
                let mut requests = Vec::new();
                for (status, kind, body) in responses {
                    let (mut socket, peer) = listener.accept().await.unwrap();
                    assert!(peer.ip().is_loopback());
                    requests.push(read(&mut socket).await);
                    let header = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    socket.write_all(header.as_bytes()).await.unwrap();
                    socket.write_all(body.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
                requests
            }).await.expect("usage fixture exceeded deadline")
        });
        Self { base, tasks }
    }
    async fn finish(mut self) -> Vec<Value> {
        self.tasks.join_next().await.unwrap().unwrap()
    }
}
async fn read(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break index + 4;
        }
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0 && bytes.len() + n <= 256 * 1024);
        bytes.extend_from_slice(&chunk[..n]);
    };
    let size = std::str::from_utf8(&bytes[..end])
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .unwrap()
        .1
        .trim()
        .parse::<usize>()
        .unwrap();
    assert!(end + size <= 256 * 1024);
    while bytes.len() < end + size {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&bytes[end..end + size]).unwrap()
}
fn configuration(ai: AIConfig) -> Arc<ConfigManager> {
    Arc::new(ConfigManager::for_test(
        NuonoConfig {
            ai,
            ..Default::default()
        },
        std::env::temp_dir().join("usage-fixture.toml"),
    ))
}
async fn model(db: &Database, name: &str, base: &str) -> i64 {
    db.create_ai_model(CreateAiModel {
        name,
        provider: "fixture",
        provider_type: "openai_compatible",
        model_id: name,
        api_key: Some("local-fake-key"),
        base_url: base,
        is_default: false,
    })
    .await
    .unwrap()
    .id
}
fn completion(message: Value, tokens: u64) -> (u16, &'static str, String) {
    (200, "application/json", json!({"model":"fixture", "choices":[{"message":message,"finish_reason":"stop"}],"usage":{"prompt_tokens":tokens-2,"completion_tokens":2,"total_tokens":tokens}}).to_string())
}

#[tokio::test]
async fn independent_model_chat_and_stream_both_persist_real_usage_with_stable_identity() {
    let fixture = Fixture::new(vec![
        completion(json!({"content":"ok"}), 12),
        (200, "text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n".into()),
    ]).await;
    let db = nuono_core::test_support::test_db(2).await;
    let id = model(&db, "standalone", &fixture.base).await;
    let config = configuration(AIConfig::default()); // 即使全局 AI 开关关闭，显式连通性测试依然可用。
    let (provider, _) =
        nuono_core::service::ai_models::build_chat_provider(&db, &config, Some(id), None)
            .await
            .unwrap();
    let request = ChatRequest::new(vec![ChatMessage::user("PRIVATE_PROMPT")]);
    provider.chat(request.clone()).await.unwrap();
    let mut stream = provider.chat_stream(request).await.unwrap();
    while stream.recv().await.is_some() {}
    let summary = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let summary = db.ai_usage_summary(7).await.unwrap();
            if summary.totals.requests == 2 {
                break summary;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(summary.totals.requests, 2);
    assert_eq!(summary.totals.total_tokens, Some(24));
    assert_eq!(summary.totals.known_usage_requests, 2);
    assert_eq!(summary.unrecorded_requests, 0);
    assert_eq!(summary.models.len(), 2); // 普通对话与流式对话的用量记录仍可区分。
    for row in summary.models {
        assert_eq!(row.model_id, Some(id));
        assert_eq!(row.source, "model_test");
        assert_eq!(row.name, "standalone");
    }
    assert_eq!(fixture.finish().await.len(), 2);
    db.shutdown().await.unwrap();
}

struct Echo;
#[async_trait::async_trait]
impl AgentTool for Echo {
    fn name(&self) -> &str {
        "fixture.echo"
    }
    fn description(&self) -> &str {
        "Return local fixture data"
    }
    async fn call(&self, _: AgentToolContext, _: Value) -> nuono_core::error::Result<Value> {
        Ok(json!({"fixture":"tool result"}))
    }
}

#[tokio::test]
async fn agent_fallback_and_tool_rounds_count_every_attempt_without_changing_task_budget() {
    let unavailable = Fixture::new(vec![(
        503,
        "application/json",
        r#"{"error":{"message":"fixture unavailable"}}"#.into(),
    )])
    .await;
    let working = Fixture::new(vec![
        completion(json!({"content":null,"tool_calls":[{"id":"call1","type":"function","function":{"name":"fixture.echo","arguments":"{}"}}]}), 10),
        completion(json!({"content":"finished"}), 20),
    ]).await;
    let db = nuono_core::test_support::test_db(2).await;
    let first = model(&db, "unavailable", &unavailable.base).await;
    let backup = model(&db, "working", &working.base).await;
    let config = configuration(AIConfig {
        enabled: true,
        reply_enabled: true,
        chat_model_alias: format!("id:{first}"),
        tool_model_alias: format!("id:{backup}"),
        provider_fallback_chain: vec![format!("id:{backup}")],
        provider_health_fuse_enabled: false,
        agent_autonomy: "low_risk_tools".into(),
        ..Default::default()
    });
    let tools = Arc::new(ToolRegistry::new());
    tools.register(Echo).unwrap();
    let runtime = AgentRuntime::with_tool_registry(db.clone(), config, tools);
    let ctx = AgentContext::from_runtime_event(42, &json!({"post_type":"message","user_id":7,"message_type":"private","raw_message":"fixture task","message":"fixture task"})).unwrap();
    let reply = runtime
        .submit_text(ctx)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(reply.content, "finished");
    let run = runtime.get_run(&reply.run_id).await.unwrap().unwrap();
    assert_eq!(run.tokens, 30); // 原有基于模型上报用量的任务计费统计保持不变。
    assert_eq!(run.rounds, 2);
    let summary = db.ai_usage_summary(7).await.unwrap();
    assert_eq!(summary.totals.requests, 3);
    assert_eq!(summary.totals.successes, 2);
    assert_eq!(summary.totals.failures, 1);
    assert_eq!(summary.totals.unknown_usage_requests, 1);
    assert_eq!(summary.totals.total_tokens, Some(30));
    assert_eq!(summary.unrecorded_requests, 0);
    assert!(summary.models.iter().all(|row| row.source == "agent"));
    assert_eq!(
        summary
            .models
            .iter()
            .find(|row| row.model_id == Some(backup))
            .unwrap()
            .counts
            .requests,
        2
    );
    assert_eq!(unavailable.finish().await.len(), 1);
    let requests = working.finish().await;
    assert_eq!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        1
    );
    db.shutdown().await.unwrap();
}
