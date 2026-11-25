//! 使用本地回环 HTTP 测试服务对 Responses 传输层做回归测试。
use super::test_http::{
    HttpFixture, finish_chunks, wait_for_client_close, write_chunk, write_sse_headers,
};
use super::*;
use nuono_ai::ChatMessage;
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

fn provider(base: &str) -> OpenAiCompatibleProvider {
    let mut provider =
        OpenAiCompatibleProvider::new("local-test-key", base, ProviderType::Responses).unwrap();
    provider.client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    provider
}

fn request() -> ChatRequest {
    ChatRequest::new(vec![ChatMessage::user("本地测试")]).with_model("local-responses-model")
}

async fn next(
    receiver: &mut mpsc::Receiver<Result<ChatStreamChunk, AiError>>,
) -> Option<Result<ChatStreamChunk, AiError>> {
    tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("Responses stream must make progress without waiting for the HTTP deadline")
}

fn completed() -> Vec<u8> {
    format!(
        "data: {}\r\n\r\n",
        json!({
            "type": "response.completed",
            "response": {
                "id": "response-local", "model": "local-responses-model", "status": "completed",
            },
        })
    )
    .into_bytes()
}

#[tokio::test]
async fn responses_sse_decodes_multiline_data_metadata_and_fragmented_utf8() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        let event = concat!(
            "\u{feff}: heartbeat\r\n",
            "event: response.output_text.delta\r\n",
            "id: ignored-metadata\r\n",
            "retry: 1000\r\n",
            "data: {\"type\":\"response.output_text.delta\",\r\n",
            "data: \"delta\":\"你好🙂\"}\r\n\r\n",
        )
        .as_bytes();
        let split = event.windows(3).position(|bytes| bytes == "你".as_bytes()).unwrap();
        write_chunk(&mut socket, &event[..split + 1]).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        write_chunk(&mut socket, &event[split + 1..split + 2]).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        write_chunk(&mut socket, &event[split + 2..]).await;
        // 单独的 `\r` 也是合法的 SSE 行分隔符；注释和元数据行绝不能混入正文输出。
        write_chunk(
            &mut socket,
            b": heartbeat\revent: response.refusal.delta\rdata: {\"type\":\"response.refusal.delta\",\"delta\":\"!\"}\r\r",
        )
        .await;
        write_chunk(&mut socket, &completed()).await;
        wait_for_client_close(&mut socket).await;
    })
    .await;
    let mut stream = provider(fixture.base_url())
        .chat_stream(request())
        .await
        .unwrap();
    let first = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(first.delta_content, "你好🙂");
    assert!(first.finish_reason.is_none());
    let second = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(second.delta_content, "!");
    let last = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(last.id, "response-local");
    assert_eq!(last.model, "local-responses-model");
    assert_eq!(last.finish_reason.as_deref(), Some("stop"));
    assert!(next(&mut stream).await.is_none());
    let wire = fixture.finish().await;
    assert_eq!(wire.method, "POST");
    assert_eq!(wire.path, "/v1/responses");
    assert_eq!(wire.header("authorization"), Some("Bearer local-test-key"));
    assert_eq!(wire.body["model"], "local-responses-model");
    assert_eq!(wire.body["stream"], true);
    assert_eq!(wire.body["store"], false);
    assert_eq!(wire.body["input"][0]["content"], "本地测试");
}

#[tokio::test]
async fn responses_completed_closes_receiver_and_http_without_waiting_for_server_eof() {
    let (closed_tx, closed_rx) = oneshot::channel();
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(&mut socket, &completed()).await;
        // 保持 chunked 响应连接不关闭，验证客户端收到 completed 事件后会主动断开连接。
        wait_for_client_close(&mut socket).await;
        closed_tx.send(()).unwrap();
    })
    .await;
    let mut stream = provider(fixture.base_url())
        .chat_stream(request())
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream)
            .await
            .unwrap()
            .unwrap()
            .finish_reason
            .as_deref(),
        Some("stop")
    );
    assert!(next(&mut stream).await.is_none());
    tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("completed Responses stream must close the HTTP connection")
        .unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn responses_failure_events_are_visible_after_already_delivered_text() {
    for kind in ["response.failed", "response.incomplete", "error"] {
        let body = format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\n\
             data: {}\n\n",
            json!({"type": kind, "error": {"message": "mock stream failure"}}),
        );
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            write_chunk(&mut socket, body.as_bytes()).await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let mut stream = provider(fixture.base_url())
            .chat_stream(request())
            .await
            .unwrap();
        assert_eq!(
            next(&mut stream).await.unwrap().unwrap().delta_content,
            "partial"
        );
        assert!(
            matches!(next(&mut stream).await.unwrap(), Err(AiError::RequestFailed(message))
            if message.contains("mock stream failure")),
            "{kind} must be reported as an error"
        );
        assert!(next(&mut stream).await.is_none());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn responses_completed_with_failed_or_incomplete_status_is_not_success() {
    for status in ["failed", "incomplete"] {
        let body = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {"status": status, "error": {"message": "mock terminal failure"}},
            }),
        );
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            write_chunk(&mut socket, body.as_bytes()).await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let mut stream = provider(fixture.base_url())
            .chat_stream(request())
            .await
            .unwrap();
        assert!(
            matches!(
                next(&mut stream).await.unwrap(),
                Err(AiError::RequestFailed(_))
            ),
            "terminal response status {status} must not be reported as stop"
        );
        assert!(next(&mut stream).await.is_none());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn responses_stream_rejects_malformed_json_and_invalid_utf8() {
    for body in [
        b"data: {broken json}\n\n".to_vec(),
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\xff\"}\n\n".to_vec(),
    ] {
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            write_chunk(&mut socket, &body).await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let mut stream = provider(fixture.base_url())
            .chat_stream(request())
            .await
            .unwrap();
        assert!(matches!(
            next(&mut stream).await.unwrap(),
            Err(AiError::ResponseParseFailed(_))
        ));
        assert!(next(&mut stream).await.is_none());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn responses_normal_http_eof_without_completed_is_an_error() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        )
        .await;
        write_chunk(&mut socket, b"data: [DONE]\n\n").await;
        finish_chunks(&mut socket).await;
    })
    .await;
    let mut stream = provider(fixture.base_url())
        .chat_stream(request())
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream).await.unwrap().unwrap().delta_content,
        "partial"
    );
    assert!(matches!(
        next(&mut stream).await.unwrap(),
        Err(AiError::RequestFailed(_))
    ));
    assert!(next(&mut stream).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn responses_truncated_http_body_reports_transport_error_after_partial_text() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        )
        .await;
        // HTTP chunk 声明了 64 字节（0x40），但在发送完整前直接断开连接。
        socket.write_all(b"40\r\ndata: {\"type\":").await.unwrap();
        socket.shutdown().await.unwrap();
    })
    .await;
    let mut stream = provider(fixture.base_url())
        .chat_stream(request())
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream).await.unwrap().unwrap().delta_content,
        "partial"
    );
    assert!(matches!(
        next(&mut stream).await.unwrap(),
        Err(AiError::RequestFailed(_))
    ));
    assert!(next(&mut stream).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn responses_receiver_drop_cancels_an_otherwise_idle_http_stream() {
    let (closed_tx, closed_rx) = oneshot::channel();
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n",
        )
        .await;
        // 服务端不再发送新字节，验证丢弃接收端时无需等待下一次数据到达即可立即取消连接。
        wait_for_client_close(&mut socket).await;
        closed_tx.send(()).unwrap();
    })
    .await;
    let mut stream = provider(fixture.base_url())
        .chat_stream(request())
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream).await.unwrap().unwrap().delta_content,
        "first"
    );
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("dropping the receiver must release idle HTTP before its 3-second deadline")
        .unwrap();
    fixture.finish().await;
}
