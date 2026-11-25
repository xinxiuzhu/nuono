//! 使用本地回环 HTTP 测试服务对 Chat Completions SSE 流式传输做回归测试。
use super::test_http::{
    HttpFixture, finish_chunks, wait_for_client_close, write_chunk, write_sse_headers,
};
use super::{
    AiError, AiService, ChatRequest, ChatStreamChunk, OpenAiCompatibleProvider, ProviderType,
};
use nuono_ai::ChatMessage;
use serde_json::json;
use std::time::Duration;

fn provider(fixture: &HttpFixture) -> OpenAiCompatibleProvider {
    let mut provider = OpenAiCompatibleProvider::new(
        "mock-key",
        fixture.base_url(),
        ProviderType::OpenAiCompatible,
    )
    .unwrap()
    .model("mock-model")
    .max_tokens(64)
    .temperature(0.25);
    provider.client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    provider
}

fn request() -> ChatRequest {
    ChatRequest::new(vec![
        ChatMessage::system("记住名字"),
        ChatMessage::user("我叫小明"),
    ])
}

async fn receive(
    rx: &mut tokio::sync::mpsc::Receiver<Result<ChatStreamChunk, AiError>>,
) -> Option<Result<ChatStreamChunk, AiError>> {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("stream did not finish promptly")
}

#[tokio::test]
async fn fragmented_unicode_is_preserved_and_stream_request_is_correct() {
    let fixture = HttpFixture::spawn(|mut socket, recorded| async move {
        assert_eq!(recorded.method, "POST");
        assert_eq!(recorded.path, "/v1/chat/completions");
        assert_eq!(recorded.header("authorization"), Some("Bearer mock-key"));
        assert_eq!(recorded.body, json!({
            "model":"mock-model", "stream":true, "max_tokens":64, "temperature":0.25,
            "messages":[{"role":"system","content":"记住名字"},{"role":"user","content":"我叫小明"}]
        }));
        write_sse_headers(&mut socket).await;
        let event = "data: {\"id\":\"chunk-1\",\"model\":\"wire-model\",\"choices\":[{\"delta\":{\"content\":\"你好🙂\"},\"finish_reason\":null}]}\n\n";
        let split = event.find('你').unwrap() + 1;
        write_chunk(&mut socket, &event.as_bytes()[..split]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        write_chunk(&mut socket, &event.as_bytes()[split..]).await;
        write_chunk(&mut socket, b"data: [DONE]\n\n").await;
        wait_for_client_close(&mut socket).await;
    }).await;
    let mut rx = provider(&fixture).chat_stream(request()).await.unwrap();
    let chunk = receive(&mut rx).await.unwrap().unwrap();
    assert_eq!(chunk.delta_content, "你好🙂");
    assert_eq!(chunk.id, "chunk-1");
    assert_eq!(chunk.model, "wire-model");
    assert_eq!(chunk.finish_reason, None);
    assert!(receive(&mut rx).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn multiline_sse_and_metadata_are_decoded_as_one_event() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(&mut socket, concat!(
            "\u{feff}: heartbeat\r\nevent: message\r\nid: event-1\r\nretry: 1000\r\n",
            "data: {\"id\":\"chunk-2\",\r\n",
            "data: \"choices\":[{\"delta\":{\"content\":\"小明\"},\"finish_reason\":null}]}\r\n\r\n",
            "data: [DONE]\r\r",
        ).as_bytes()).await;
        wait_for_client_close(&mut socket).await;
    }).await;
    let mut rx = provider(&fixture).chat_stream(request()).await.unwrap();
    let chunk = receive(&mut rx).await.unwrap().unwrap();
    assert_eq!(chunk.delta_content, "小明");
    assert_eq!(chunk.model, "mock-model");
    assert!(receive(&mut rx).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn premature_clean_eof_is_reported_after_partial_content() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(&mut socket, b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n").await;
        finish_chunks(&mut socket).await;
    }).await;
    let mut rx = provider(&fixture).chat_stream(request()).await.unwrap();
    assert_eq!(
        receive(&mut rx).await.unwrap().unwrap().delta_content,
        "partial"
    );
    assert!(matches!(
        receive(&mut rx).await,
        Some(Err(AiError::RequestFailed(_)))
    ));
    assert!(receive(&mut rx).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn invalid_stream_payloads_produce_one_error_and_close() {
    for payload in [
        b"data: {bad json}\n\n".as_slice(),
        b"data: {\"choices\":[{\"delta\":{\"content\":\"\xff\"}}]}\n\n".as_slice(),
    ] {
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            write_chunk(&mut socket, payload).await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let mut rx = provider(&fixture).chat_stream(request()).await.unwrap();
        assert!(matches!(
            receive(&mut rx).await,
            Some(Err(AiError::ResponseParseFailed(_)))
        ));
        assert!(receive(&mut rx).await.is_none());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn server_error_inside_successful_http_stream_is_visible() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            b"data: {\"error\":{\"message\":\"mock upstream failed\"}}\n\n",
        )
        .await;
        wait_for_client_close(&mut socket).await;
    })
    .await;
    let mut rx = provider(&fixture).chat_stream(request()).await.unwrap();
    let error = receive(&mut rx).await.unwrap().unwrap_err();
    assert!(
        matches!(&error, AiError::RequestFailed(message) if message.contains("mock upstream failed"))
    );
    assert!(receive(&mut rx).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn finish_reason_ends_stream_without_waiting_for_done_or_eof() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        write_chunk(&mut socket, b"data: {\"choices\":[{\"delta\":{\"content\":\"finished\"},\"finish_reason\":\"stop\"}]}\n\n").await;
        wait_for_client_close(&mut socket).await;
    }).await;
    let mut rx = provider(&fixture).chat_stream(request()).await.unwrap();
    let chunk = receive(&mut rx).await.unwrap().unwrap();
    assert_eq!(chunk.delta_content, "finished");
    assert_eq!(chunk.finish_reason.as_deref(), Some("stop"));
    assert!(receive(&mut rx).await.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn dropping_receiver_cancels_idle_http_stream() {
    let fixture = HttpFixture::spawn(|mut socket, _| async move {
        write_sse_headers(&mut socket).await;
        wait_for_client_close(&mut socket).await;
    })
    .await;
    let rx = provider(&fixture).chat_stream(request()).await.unwrap();
    drop(rx);
    tokio::time::timeout(Duration::from_secs(1), fixture.finish())
        .await
        .expect("HTTP stream was not cancelled");
}
