use super::*;
use crate::agent::output::OutputProgress;
use crate::agent::runtime::ProviderRouteScenario;
use std::collections::VecDeque;

struct StreamProvider {
    plans: Mutex<VecDeque<Vec<std::result::Result<ChatStreamChunk, AiError>>>>,
    streams: AtomicUsize,
    buffered: AtomicUsize,
    hold: AtomicBool,
    held_sender:
        Mutex<Option<tokio::sync::mpsc::Sender<std::result::Result<ChatStreamChunk, AiError>>>>,
}
#[async_trait::async_trait]
impl AiService for StreamProvider {
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, AiError> {
        self.buffered.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            id: "buffered".into(),
            model: "test".into(),
            content: "buffered".into(),
            usage: UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
            finish_reason: Some("stop".into()),
            tool_calls: vec![],
            continuation: vec![],
        })
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<ChatStreamChunk, AiError>>,
        AiError,
    > {
        assert!(request.tools.is_empty());
        self.streams.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        if self.hold.load(Ordering::SeqCst) {
            *self.held_sender.lock().unwrap() = Some(tx);
            return Ok(rx);
        }
        for item in self.plans.lock().unwrap().pop_front().expect("stream plan") {
            tx.try_send(item).unwrap();
        }
        Ok(rx)
    }
    fn service_name(&self) -> &str {
        "stream-fixture"
    }
}
struct StreamFactory(Arc<StreamProvider>);
impl ModelProviderFactory for StreamFactory {
    fn create(&self, _: &AiConfig) -> std::result::Result<Arc<dyn AiService>, AiError> {
        Ok(self.0.clone())
    }
}
fn chunk(text: &str, finished: bool) -> std::result::Result<ChatStreamChunk, AiError> {
    Ok(ChatStreamChunk {
        id: "stream".into(),
        model: "test".into(),
        delta_content: text.into(),
        finish_reason: finished.then(|| "stop".into()),
    })
}
fn fixture(
    plans: Vec<Vec<std::result::Result<ChatStreamChunk, AiError>>>,
) -> (AgentRuntime, Arc<StreamProvider>) {
    let provider = Arc::new(StreamProvider {
        plans: Mutex::new(plans.into()),
        streams: AtomicUsize::new(0),
        buffered: AtomicUsize::new(0),
        hold: AtomicBool::new(false),
        held_sender: Mutex::new(None),
    });
    let mut cfg = Config::default();
    cfg.0.get_mut().unwrap().agent_autonomy = "reply_only".into();
    let runtime = AgentRuntime::with_ports(
        Arc::new(Store::default()),
        Arc::new(cfg),
        Arc::new(StreamFactory(provider.clone())),
        Arc::new(ToolRegistry::new()),
    );
    (runtime, provider)
}
#[tokio::test]
async fn reply_only_run_publishes_a_completed_prefix_and_keeps_run_ownership() {
    let (runtime, provider) = fixture(vec![vec![chunk("诺", false), chunk("诺🙂", true)]]);
    let (tx, rx) = tokio::sync::watch::channel(OutputProgress::default());
    let task = runtime
        .submit_text_with_output(context(2, "hi"), tx)
        .await
        .unwrap()
        .unwrap();
    let reply = task.await.unwrap();
    assert_eq!(reply.content, "诺诺🙂");
    assert!(rx.borrow().complete);
    assert_eq!(rx.borrow().content, reply.content);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 1);
    assert_eq!(provider.buffered.load(Ordering::SeqCst), 0);
    runtime.wait_for_idle().await.unwrap();
}
#[tokio::test]
async fn tool_autonomy_keeps_the_existing_buffered_path_even_with_an_observer() {
    let (mut runtime, provider) = fixture(vec![]);
    runtime.config = Arc::new(Config::default());
    let (tx, rx) = tokio::sync::watch::channel(OutputProgress::default());
    let reply = runtime
        .submit_text_with_output(context(2, "hi"), tx)
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_eq!(reply.content, "buffered");
    assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
    assert!(!rx.borrow().complete);
}
#[tokio::test]
async fn provider_fallback_is_allowed_only_before_a_prefix_is_published() {
    for published in [false, true] {
        let mut first = vec![];
        if published {
            first.push(chunk("旧开头", false));
        }
        first.push(Err(AiError::RequestFailed("interrupted".into())));
        let (runtime, provider) = fixture(vec![first, vec![chunk("新模型答案", true)]]);
        let cfg = runtime.config.ai_settings().await;
        let ai_cfg = cfg.to_ai_config().unwrap();
        let mut candidates = runtime
            .build_provider_candidates(&cfg, &ai_cfg, ProviderRouteScenario::Chat)
            .await;
        candidates.extend(
            runtime
                .build_provider_candidates(
                    &cfg,
                    &AiConfig {
                        model: "fallback".into(),
                        ..ai_cfg
                    },
                    ProviderRouteScenario::Chat,
                )
                .await,
        );
        let (tx, rx) = tokio::sync::watch::channel(OutputProgress::default());
        let result = runtime
            .chat_with_fallback_output(
                &context(2, "hi"),
                "session",
                candidates,
                ChatRequest::new(vec![ChatMessage::user("hi")]),
                1,
                false,
                Some(&tx),
            )
            .await;
        assert_eq!(
            provider.streams.load(Ordering::SeqCst),
            if published { 1 } else { 2 }
        );
        assert_eq!(result.is_err(), published);
        assert_eq!(
            rx.borrow().content,
            if published {
                "旧开头"
            } else {
                "新模型答案"
            }
        );
        assert_eq!(rx.borrow().complete, !published);
    }
}
#[tokio::test]
async fn truncated_and_oversized_model_streams_never_claim_completion() {
    for plan in [
        vec![chunk("partial", false)],
        vec![chunk(
            &"x".repeat(crate::agent::output::MAX_OUTPUT_BYTES + 1),
            true,
        )],
    ] {
        let (runtime, _) = fixture(vec![plan]);
        let (tx, rx) = tokio::sync::watch::channel(OutputProgress::default());
        let reply = runtime
            .submit_text_with_output(context(2, "hi"), tx)
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap();
        assert!(reply.content.contains("模型调用失败"));
        assert!(!rx.borrow().complete);
        assert!(rx.borrow().content.len() <= crate::agent::output::MAX_OUTPUT_BYTES);
    }
}

#[tokio::test]
async fn cancellation_closes_the_provider_receiver_and_releases_its_model_permit() {
    let (runtime, provider) = fixture(vec![]);
    provider.hold.store(true, Ordering::SeqCst);
    let baseline = runtime.resources.models.available_permits();
    let (tx, rx) = tokio::sync::watch::channel(OutputProgress::default());
    let task = runtime
        .submit_text_with_output(context(2, "hi"), tx)
        .await
        .unwrap()
        .unwrap();
    let worker = tokio::spawn(task);
    until(|| {
        provider.streams.load(Ordering::SeqCst) == 1
            && provider.held_sender.lock().unwrap().is_some()
    })
    .await;
    assert_eq!(runtime.resources.models.available_permits(), baseline - 1);
    let session = runtime.runs.iter().next().unwrap().key().clone();
    let run = runtime
        .db
        .active_agent_run(&session)
        .await
        .unwrap()
        .unwrap();
    runtime.cancel_run(&run.id).await.unwrap();
    assert_eq!(worker.await.unwrap().unwrap().content, "任务已取消");
    assert!(
        provider
            .held_sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_closed()
    );
    assert_eq!(runtime.resources.models.available_permits(), baseline);
    assert!(!rx.borrow().complete);
}
