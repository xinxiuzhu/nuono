use super::{
    AiError, AiService, ChatRequest, OpenAiCompatibleProvider, ProviderType,
    test_http::{
        HttpFixture, wait_for_client_close, write_chunk, write_response, write_sse_headers,
    },
};
use nuono_ai::ChatMessage;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

const SHORT_TIMEOUT: Duration = Duration::from_millis(180);
const TEST_DEADLINE: Duration = Duration::from_secs(2);
const PROTOCOLS: [ProviderType; 2] = [ProviderType::OpenAiCompatible, ProviderType::Responses];

fn provider(fixture: &HttpFixture, protocol: ProviderType) -> OpenAiCompatibleProvider {
    let mut provider =
        OpenAiCompatibleProvider::new("fixture-fake-key", fixture.base_url(), protocol)
            .unwrap()
            .model("local-mock");
    // 本地测试流量绝不能走系统环境变量里的 HTTP 代理。
    provider.client = reqwest::Client::builder()
        .no_proxy()
        .timeout(TEST_DEADLINE)
        .build()
        .unwrap();
    provider
}

fn short_timeout_provider(
    fixture: &HttpFixture,
    protocol: ProviderType,
) -> OpenAiCompatibleProvider {
    let mut provider = provider(fixture, protocol);
    provider.client = reqwest::Client::builder()
        .no_proxy()
        .timeout(SHORT_TIMEOUT)
        .build()
        .unwrap();
    provider
}

fn request() -> ChatRequest {
    ChatRequest::new(vec![ChatMessage::user("只访问本机模拟服务")])
}

async fn request_error(provider: &OpenAiCompatibleProvider, streaming: bool) -> AiError {
    tokio::time::timeout(TEST_DEADLINE, async {
        if streaming {
            provider.chat_stream(request()).await.unwrap_err()
        } else {
            provider.chat(request()).await.unwrap_err()
        }
    })
    .await
    .expect("HTTP error request exceeded test deadline")
}

fn assert_error_status(status: u16, error: &AiError) {
    assert!(
        match status {
            401 | 403 => matches!(error, AiError::InvalidApiKey),
            404 => matches!(error, AiError::ModelNotFound(_)),
            408 | 504 => matches!(error, AiError::Timeout(_)),
            429 => matches!(error, AiError::RateLimitExceeded(_)),
            _ => matches!(error, AiError::RequestFailed(_)),
        },
        "HTTP {status} was misclassified: {error:?}"
    );
}

#[tokio::test]
async fn http_status_errors_are_classified_for_both_protocols_and_request_modes() {
    for protocol in PROTOCOLS {
        for streaming in [false, true] {
            for status in [401, 403, 404, 408, 429, 500, 504] {
                let fixture = HttpFixture::spawn(move |mut socket, _| async move {
                    write_response(
                        &mut socket,
                        status,
                        "application/json",
                        r#"{"error":{"message":"fixture rejection"}}"#,
                    )
                    .await;
                })
                .await;
                let error = request_error(&provider(&fixture, protocol), streaming).await;
                assert_error_status(status, &error);
                if !matches!(status, 401 | 403) {
                    assert!(error.to_string().contains("fixture rejection"));
                }
                let sent = fixture.finish().await;
                assert_eq!(sent.method, "POST");
                assert_eq!(
                    sent.path,
                    if protocol == ProviderType::Responses {
                        "/v1/responses"
                    } else {
                        "/v1/chat/completions"
                    }
                );
                assert_eq!(
                    sent.header("authorization"),
                    Some("Bearer fixture-fake-key")
                );
                assert_eq!(sent.body["model"], "local-mock");
                assert_eq!(sent.body["stream"], streaming);
            }
        }
    }
}

#[tokio::test]
async fn non_json_error_bodies_preserve_status_and_details() {
    for protocol in PROTOCOLS {
        for streaming in [false, true] {
            for status in [404, 429, 500] {
                let fixture = HttpFixture::spawn(move |mut socket, _| async move {
                    write_response(
                        &mut socket,
                        status,
                        "text/html",
                        "<html>fixture upstream unavailable</html>",
                    )
                    .await;
                })
                .await;
                let error = request_error(&provider(&fixture, protocol), streaming).await;
                assert_error_status(status, &error);
                assert!(error.to_string().contains("fixture upstream unavailable"));
                fixture.finish().await;
            }
        }
    }
}

#[tokio::test]
async fn malformed_success_json_and_missing_response_fields_fail_explicitly() {
    for (protocol, body) in [
        (ProviderType::OpenAiCompatible, "{not-json}"),
        (ProviderType::OpenAiCompatible, "{}"),
        (ProviderType::OpenAiCompatible, r#"{"choices":[]}"#),
        (
            ProviderType::OpenAiCompatible,
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
        ),
        (ProviderType::Responses, "{not-json}"),
        (ProviderType::Responses, r#"{"status":"completed"}"#),
        (
            ProviderType::Responses,
            r#"{"status":"completed","output":[{"type":"function_call","call_id":"call_1","arguments":"{}"}]}"#,
        ),
        (
            ProviderType::Responses,
            r#"{"status":"completed","output":[{"type":"function_call","name":"lookup","arguments":"{}"}]}"#,
        ),
    ] {
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_response(&mut socket, 200, "application/json", body).await;
        })
        .await;
        let error = request_error(&provider(&fixture, protocol), false).await;
        assert!(
            matches!(error, AiError::ResponseParseFailed(_)),
            "{protocol}: {body} produced {error:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn malformed_stream_json_is_one_terminal_parse_error() {
    for protocol in PROTOCOLS {
        let fixture = HttpFixture::spawn(|mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            write_chunk(&mut socket, b"data: {broken-json}\n\n").await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let mut chunks = provider(&fixture, protocol)
            .chat_stream(request())
            .await
            .unwrap();
        let error = tokio::time::timeout(TEST_DEADLINE, chunks.recv())
            .await
            .unwrap()
            .expect("malformed stream silently closed")
            .unwrap_err();
        assert!(matches!(error, AiError::ResponseParseFailed(_)));
        assert!(
            tokio::time::timeout(TEST_DEADLINE, chunks.recv())
                .await
                .unwrap()
                .is_none()
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn actual_request_deadline_before_headers_is_classified_as_timeout() {
    for protocol in PROTOCOLS {
        for streaming in [false, true] {
            let fixture = HttpFixture::spawn(|mut socket, _| async move {
                wait_for_client_close(&mut socket).await;
            })
            .await;
            let provider = short_timeout_provider(&fixture, protocol);
            let started = Instant::now();
            let error = request_error(&provider, streaming).await;
            assert!(matches!(error, AiError::Timeout(_)), "{error:?}");
            assert!(started.elapsed() >= Duration::from_millis(100));
            fixture.finish().await;
        }
    }
}

#[tokio::test]
async fn actual_body_deadline_is_not_misclassified_or_discarded() {
    for protocol in PROTOCOLS {
        // 覆盖成功响应和错误响应在读取响应体时卡死的场景。
        // 流式请求遇到非 2xx 状态码时同样需要读取错误响应体，不能吞掉读取超时错误。
        for (status, streaming) in [(200, false), (500, false), (500, true)] {
            let fixture = HttpFixture::spawn(move |mut socket, _| async move {
                socket
                    .write_all(
                        format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                wait_for_client_close(&mut socket).await;
            })
            .await;
            let provider = short_timeout_provider(&fixture, protocol);
            let error = request_error(&provider, streaming).await;
            assert!(
                matches!(error, AiError::Timeout(_)),
                "{protocol}, streaming={streaming}, HTTP {status}: {error:?}"
            );
            fixture.finish().await;
        }
    }
}

#[tokio::test]
async fn actual_stream_deadline_after_partial_content_is_one_terminal_timeout() {
    for protocol in PROTOCOLS {
        let fixture = HttpFixture::spawn(move |mut socket, _| async move {
            write_sse_headers(&mut socket).await;
            let event = if protocol == ProviderType::Responses {
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"第一段\"}\n\n"
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"第一段\"}}]}\n\n"
            };
            write_chunk(&mut socket, event.as_bytes()).await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let mut chunks = short_timeout_provider(&fixture, protocol)
            .chat_stream(request())
            .await
            .unwrap();
        let first = tokio::time::timeout(TEST_DEADLINE, chunks.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.delta_content, "第一段");
        assert_eq!(first.model, "local-mock");
        let error = tokio::time::timeout(TEST_DEADLINE, chunks.recv())
            .await
            .unwrap()
            .expect("stalled stream silently closed")
            .unwrap_err();
        assert!(
            matches!(error, AiError::Timeout(_)),
            "{protocol}: {error:?}"
        );
        assert!(
            tokio::time::timeout(TEST_DEADLINE, chunks.recv())
                .await
                .unwrap()
                .is_none()
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn truncated_non_stream_body_is_a_parse_error_and_not_a_timeout() {
    for protocol in PROTOCOLS {
        let fixture = HttpFixture::spawn(|mut socket, _| async move {
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        })
        .await;
        let error = request_error(&provider(&fixture, protocol), false).await;
        assert!(
            matches!(error, AiError::ResponseParseFailed(_)),
            "{error:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn oversized_success_response_is_rejected_before_unbounded_buffering() {
    for protocol in PROTOCOLS {
        let fixture = HttpFixture::spawn(|mut socket, _| async move {
            let declared = super::MAX_SUCCESS_RESPONSE_BYTES + 1;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let error = request_error(&provider(&fixture, protocol), false).await;
        assert!(
            matches!(error, AiError::ResponseParseFailed(_)),
            "{protocol}: {error:?}"
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn oversized_error_response_is_capped_and_preserves_http_status() {
    for protocol in PROTOCOLS {
        let fixture = HttpFixture::spawn(|mut socket, _| async move {
            let chunk_size = 64 * 1024;
            let payload = vec![b'x'; chunk_size];
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk_header = format!("{chunk_size:x}\r\n");
            for _ in 0..8 {
                if socket.write_all(chunk_header.as_bytes()).await.is_err()
                    || socket.write_all(&payload).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            wait_for_client_close(&mut socket).await;
        })
        .await;
        let error = request_error(&provider(&fixture, protocol), false).await;
        match error {
            AiError::RequestFailed(message) => {
                assert!(message.starts_with("HTTP 502: "), "{protocol}: {message}");
                assert!(
                    message.len() <= super::MAX_ERROR_RESPONSE_BYTES + 32,
                    "{protocol}: error message len {} exceeded cap",
                    message.len()
                );
            }
            other => panic!("{protocol}: unexpected error {other:?}"),
        }
        fixture.finish().await;
    }
}
