//! 基于本地回环 HTTP 服务的用量统计测试，无需真实凭据或调用外部模型。
use super::{
    test_http::{
        HttpFixture, finish_chunks, wait_for_client_close, write_chunk, write_response,
        write_sse_headers,
    },
    *,
};
use crate::usage::UsageTracking;
use nuono_ai::{
    ChatMessage,
    usage::{UsageContext, UsageEvent, UsageObserver, UsageOutcome},
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Capture {
    events: Mutex<Vec<UsageEvent>>,
    changed: tokio::sync::Notify,
}
impl UsageObserver for Capture {
    fn observe(&self, event: UsageEvent) {
        self.events.lock().unwrap().push(event);
        self.changed.notify_one();
    }
}
impl Capture {
    async fn event(&self) -> UsageEvent {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = self.changed.notified();
                if let Some(event) = self.events.lock().unwrap().first().cloned() {
                    return event;
                }
                notified.await;
            }
        })
        .await
        .unwrap()
    }
}
fn tracked(base: &str, protocol: ProviderType, capture: Arc<Capture>) -> OpenAiCompatibleProvider {
    let mut provider = OpenAiCompatibleProvider::new("local-fake-key", base, protocol)
        .unwrap()
        .model("fixture-model")
        .with_usage_observer(UsageTracking {
            observer: capture,
            context: UsageContext {
                model_id: Some(7),
                name: "Test model".into(),
                provider: "Fixture".into(),
                source: "model_test".into(),
            },
        });
    provider.client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    provider
}
fn request() -> ChatRequest {
    ChatRequest::new(vec![ChatMessage::user("PRIVATE_PROMPT_NOT_IN_USAGE")])
}
#[tokio::test]
async fn completion_usage_records_cached_and_reasoning_subsets_once_without_content() {
    let fixture=HttpFixture::spawn(|mut socket,_|async move {write_response(&mut socket,200,"application/json",&json!({"id":"r","model":"fixture-model","choices":[{"message":{"content":"PRIVATE_RESPONSE"},"finish_reason":"stop"}],"usage":{"prompt_tokens":101,"completion_tokens":23,"total_tokens":124,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":7}}}).to_string()).await;}).await;
    let capture = Arc::new(Capture::default());
    let response = tracked(
        fixture.base_url(),
        ProviderType::OpenAiCompatible,
        capture.clone(),
    )
    .chat(request())
    .await
    .unwrap();
    assert_eq!(response.usage.total_tokens, 124); // 保持原有的 Agent token 预算字段语义不变。
    let event = capture.event().await;
    assert_eq!(event.usage.input_tokens, Some(101));
    assert_eq!(event.usage.output_tokens, Some(23));
    assert_eq!(event.usage.total_tokens, Some(124));
    assert_eq!(event.usage.cached_input_tokens, Some(40));
    assert_eq!(event.usage.reasoning_output_tokens, Some(7));
    assert_eq!(event.outcome, UsageOutcome::Succeeded);
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    let stored = serde_json::to_string(&event).unwrap();
    assert!(!stored.contains("PRIVATE_"));
    assert!(!stored.contains("fake-key"));
    assert!(!stored.contains("127.0.0.1"));
    fixture.finish().await;
}
#[tokio::test]
async fn omitted_and_malformed_usage_do_not_become_reported_zero() {
    for usage in [
        Value::Null,
        json!({"prompt_tokens":-1,"completion_tokens":"5","total_tokens":null}),
    ] {
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_response(
                &mut socket,
                200,
                "application/json",
                &json!({"choices":[{"message":{"content":"ok"}}],"usage":usage}).to_string(),
            )
            .await;
        })
        .await;
        let capture = Arc::new(Capture::default());
        let _ = tracked(
            fixture.base_url(),
            ProviderType::OpenAiCompatible,
            capture.clone(),
        )
        .chat(request())
        .await;
        assert!(!capture.event().await.usage.is_reported());
        assert_eq!(capture.events.lock().unwrap().len(), 1);
        fixture.finish().await;
    }
}
#[tokio::test]
async fn errors_and_dropped_timed_out_requests_emit_exactly_one_unknown_attempt() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_response(
            &mut socket,
            503,
            "application/json",
            r#"{"error":{"message":"fixture unavailable"}}"#,
        )
        .await;
    })
    .await;
    let capture = Arc::new(Capture::default());
    assert!(
        tracked(
            fixture.base_url(),
            ProviderType::OpenAiCompatible,
            capture.clone()
        )
        .chat(request())
        .await
        .is_err()
    );
    assert_eq!(capture.event().await.outcome, UsageOutcome::Failed);
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        wait_for_client_close(&mut socket).await;
    })
    .await;
    let capture = Arc::new(Capture::default());
    let provider = tracked(
        fixture.base_url(),
        ProviderType::OpenAiCompatible,
        capture.clone(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), provider.chat(request()))
            .await
            .is_err()
    );
    let event = capture.event().await;
    assert_eq!(event.outcome, UsageOutcome::Cancelled);
    assert!(!event.usage.is_reported());
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;
}
#[tokio::test]
async fn completion_stream_keeps_final_usage_after_finish_and_consumer_close() {
    let (release_footer, wait_for_footer) = tokio::sync::oneshot::channel();
    let fixture=HttpFixture::spawn(move |mut socket,recorded|async move {
        assert_eq!(recorded.body["stream_options"]["include_usage"],true);
        write_sse_headers(&mut socket).await;
        write_chunk(&mut socket,b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n").await;
        wait_for_footer.await.unwrap();
        write_chunk(&mut socket,b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n").await;
        wait_for_client_close(&mut socket).await;
    }).await;
    let capture = Arc::new(Capture::default());
    let mut stream = tracked(
        fixture.base_url(),
        ProviderType::OpenAiCompatible,
        capture.clone(),
    )
    .chat_stream(request())
    .await
    .unwrap();
    let last = stream.recv().await.unwrap().unwrap();
    assert_eq!(last.finish_reason.as_deref(), Some("stop"));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .unwrap()
            .is_none()
    );
    drop(stream);
    release_footer.send(()).unwrap();
    let event = capture.event().await;
    assert_eq!(event.usage.total_tokens, Some(12));
    assert_eq!(event.outcome, UsageOutcome::Succeeded);
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;
}
#[tokio::test]
async fn responses_stream_completion_usage_and_zero_reports_are_preserved() {
    let fixture=HttpFixture::spawn(|mut socket,_|async move {
        write_sse_headers(&mut socket).await;
        write_chunk(&mut socket,format!("data: {}\n\n",json!({"type":"response.completed","response":{"status":"completed","id":"r","usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":2}}}})).as_bytes()).await;
        wait_for_client_close(&mut socket).await;
    }).await;
    let capture = Arc::new(Capture::default());
    let mut stream = tracked(fixture.base_url(), ProviderType::Responses, capture.clone())
        .chat_stream(request())
        .await
        .unwrap();
    while stream.recv().await.is_some() {}
    let event = capture.event().await;
    assert_eq!(event.usage.total_tokens, Some(13));
    assert_eq!(event.usage.cached_input_tokens, Some(0));
    assert_eq!(event.usage.reasoning_output_tokens, Some(2));
    assert_eq!(event.outcome, UsageOutcome::Succeeded);
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn responses_nonstream_and_cancelled_stream_both_emit_one_actual_attempt() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_response(&mut socket, 200, "application/json", &json!({
            "id":"r", "status":"completed", "model":"fixture-model",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
            "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
        }).to_string()).await;
    }).await;
    let capture = Arc::new(Capture::default());
    tracked(fixture.base_url(), ProviderType::Responses, capture.clone())
        .chat(request())
        .await
        .unwrap();
    let event = capture.event().await;
    assert_eq!(event.usage.total_tokens, Some(6));
    assert_eq!(event.outcome, UsageOutcome::Succeeded);
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;

    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        )
        .await;
        wait_for_client_close(&mut socket).await;
    })
    .await;
    let capture = Arc::new(Capture::default());
    let mut stream = tracked(
        fixture.base_url(),
        ProviderType::OpenAiCompatible,
        capture.clone(),
    )
    .chat_stream(request())
    .await
    .unwrap();
    assert!(stream.recv().await.unwrap().is_ok());
    drop(stream);
    let event = capture.event().await;
    assert_eq!(event.outcome, UsageOutcome::Cancelled);
    assert!(!event.usage.is_reported());
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;
}
#[tokio::test]
async fn embedding_uses_reported_input_and_total_without_inventing_output() {
    let fixture=HttpFixture::spawn(|mut socket,_|async move {write_response(&mut socket,200,"application/json",r#"{"model":"fixture-model","data":[{"index":0,"embedding":[0.1,0.9]}],"usage":{"prompt_tokens":9,"total_tokens":9}}"#).await;}).await;
    let capture = Arc::new(Capture::default());
    let provider = tracked(
        fixture.base_url(),
        ProviderType::OpenAiCompatible,
        capture.clone(),
    );
    let vectors = crate::embedding::embed_with_usage(
        &provider.client,
        fixture.base_url(),
        "fixture-model",
        "local-fake-key",
        &["text".into()],
        provider.usage_tracking.clone(),
    )
    .await
    .unwrap();
    assert_eq!(vectors.len(), 1);
    let event = capture.event().await;
    assert_eq!(event.usage.input_tokens, Some(9));
    assert_eq!(event.usage.total_tokens, Some(9));
    assert_eq!(event.usage.output_tokens, None);
    assert_eq!(event.operation, nuono_ai::usage::UsageOperation::Embedding);
    fixture.finish().await;
}
#[tokio::test]
async fn stream_failure_and_premature_end_do_not_silently_report_success() {
    for body in [
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"fixture failure\"}}\n\n",
    ] {
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            write_chunk(&mut socket, body.as_bytes()).await;
            finish_chunks(&mut socket).await;
        })
        .await;
        let capture = Arc::new(Capture::default());
        let mut stream = tracked(
            fixture.base_url(),
            ProviderType::OpenAiCompatible,
            capture.clone(),
        )
        .chat_stream(request())
        .await
        .unwrap();
        let mut failed = false;
        while let Some(part) = stream.recv().await {
            failed |= part.is_err();
        }
        assert!(failed);
        let event = capture.event().await;
        assert_eq!(event.outcome, UsageOutcome::Failed);
        assert!(!event.usage.is_reported());
        assert_eq!(capture.events.lock().unwrap().len(), 1);
        fixture.finish().await;
    }
}

#[test]
fn only_explicit_option_rejections_allow_a_retry() {
    for status in [400, 422] {
        assert!(unsupported_usage_option(
            status,
            r#"{"error":{"param":"stream_options","code":"unsupported_parameter","message":"Not supported"}}"#
        ));
        assert!(unsupported_usage_option(
            status,
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#
        ));
        assert!(!unsupported_usage_option(
            status,
            r#"{"error":{"param":"temperature","code":"unsupported_parameter","message":"Not supported"}}"#
        ));
        assert!(!unsupported_usage_option(
            status,
            r#"{"error":{"message":"Unknown model while stream_options is enabled"}}"#
        ));
        assert!(!unsupported_usage_option(
            status,
            r#"{"error":{"param":"include_usage","message":"Invalid value, expected boolean"}}"#
        ));
        assert!(!unsupported_usage_option(
            status,
            "unsupported parameter stream_options"
        ));
        assert!(!unsupported_usage_option(
            status,
            r#"{"error":{"message":"unknown parameter include_usage_extra"}}"#
        ));
    }
    for status in [401, 403, 408, 429, 500, 502, 503] {
        assert!(!unsupported_usage_option(
            status,
            r#"{"error":{"param":"stream_options","code":"unsupported_parameter"}}"#
        ));
    }
}

#[tokio::test]
async fn rejected_usage_option_retries_once_without_it_and_records_both_wire_attempts() {
    use tokio::{net::TcpListener, task::JoinSet};
    for first_status in [400, 422] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async move {
                let mut requests = Vec::new();
                for attempt in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    requests.push(super::test_http::read_request(&mut socket).await.body);
                    if attempt == 0 {
                        write_response(&mut socket, first_status, "application/json", r#"{"error":{"param":"stream_options","code":"unsupported_parameter","message":"Not supported"}}"#).await;
                    } else {
                        write_response(&mut socket,200,"text/event-stream","data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n").await;
                    }
                }
                requests
            }).await.unwrap()
        });
        let capture = Arc::new(Capture::default());
        let mut stream = tracked(&base, ProviderType::OpenAiCompatible, capture.clone())
            .chat_stream(request())
            .await
            .unwrap();
        while let Some(chunk) = stream.recv().await {
            chunk.unwrap();
        }
        let requests = tasks.join_next().await.unwrap().unwrap();
        assert_eq!(requests[0]["stream_options"]["include_usage"], true);
        assert!(requests[1].get("stream_options").is_none());
        let mut expected = requests[0].clone();
        expected.as_object_mut().unwrap().remove("stream_options");
        assert_eq!(expected, requests[1]);
        tokio::time::timeout(Duration::from_secs(1), async {
            while capture.events.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let events = capture.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].outcome, UsageOutcome::Failed);
        assert!(!events[0].usage.is_reported());
        assert_eq!(events[1].outcome, UsageOutcome::Succeeded);
        assert_eq!(events[1].usage.total_tokens, Some(12));
        assert_ne!(events[0].request_id, events[1].request_id);
    }
}

#[tokio::test]
async fn generic_bad_requests_are_not_retried() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_response(
            &mut socket,
            400,
            "application/json",
            r#"{"error":{"message":"unknown model"}}"#,
        )
        .await;
    })
    .await;
    let capture = Arc::new(Capture::default());
    assert!(
        tracked(
            fixture.base_url(),
            ProviderType::OpenAiCompatible,
            capture.clone()
        )
        .chat_stream(request())
        .await
        .is_err()
    );
    assert_eq!(capture.events.lock().unwrap().len(), 1);
    fixture.finish().await;
}
