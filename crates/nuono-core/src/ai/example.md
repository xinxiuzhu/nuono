AI 服务模块。

支持 OpenAI 兼容的 Chat Completions 和 Responses API，包括 OpenAI、DeepSeek、
通义千问、智谱 AI、Moonshot/Kimi，以及 Ollama 等本地部署服务。

# 用法

传入模型配置，使用同一会话的历史发起多轮对话。文档测试使用本机临时模拟服务，
实际发送两轮 HTTP 请求并核对上下文，不需要真实模型或密钥。

```rust
use nuono_core::ai::{
    AiConfig, AiError, AiService, ChatRequest, ConversationManager, OpenAiCompatibleProvider,
};

async fn chat_two_rounds(config: &AiConfig) -> Result<(), AiError> {
    let provider = OpenAiCompatibleProvider::from_config(config)?;
    let mut conv = ConversationManager::new(10)
        .with_system_prompt("你是一个友好的助手");

    conv.add_user_message("session:1", "我叫小明");
    let first = provider.chat(ChatRequest::new(conv.get_messages("session:1"))).await?;
    println!("AI: {}", first.content);
#   check_response(&first, 0);
    conv.add_assistant_message("session:1", &first.content);

    conv.add_user_message("session:1", "我叫什么？");
    let second = provider.chat(ChatRequest::new(conv.get_messages("session:1"))).await?;
    println!("AI: {}", second.content);
#   check_response(&second, 1);
    conv.add_assistant_message("session:1", &second.content);
#   assert_eq!(conv.get_messages("session:1").len(), 5);
#   assert_eq!(conv.get_messages("session:2").len(), 1);
    Ok(())
}
#
# const REPLIES: [&str; 2] = ["你好，小明！", "你叫小明。"];
# const USAGE: [(u32, u32, u32); 2] = [(8, 4, 12), (16, 5, 21)];
# type TestResult<T> = Result<T, Box<dyn std::error::Error>>;
#
# fn check_response(response: &nuono_core::ai::ChatResponse, round: usize) {
#     assert_eq!(response.id, format!("mock-{}", round + 1));
#     assert_eq!(response.model, "mock-response-model");
#     assert_eq!(response.content, REPLIES[round]);
#     assert_eq!(response.finish_reason.as_deref(), Some("stop"));
#     assert_eq!(response.usage.prompt_tokens, USAGE[round].0);
#     assert_eq!(response.usage.completion_tokens, USAGE[round].1);
#     assert_eq!(response.usage.total_tokens, USAGE[round].2);
#     assert!(response.tool_calls.is_empty());
#     assert!(response.continuation.is_empty());
# }
#
# async fn mock(listener: tokio::net::TcpListener) -> TestResult<()> {
#     use tokio::io::{AsyncReadExt, AsyncWriteExt};
#     for round in 0..2 {
#         let (mut socket, peer) = listener.accept().await?;
#         assert!(peer.ip().is_loopback());
#         let mut bytes = Vec::new();
#         let header_end = loop {
#             if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
#                 break offset + 4;
#             }
#             let mut chunk = [0; 4096];
#             let count = socket.read(&mut chunk).await?;
#             assert!(count > 0, "request ended before headers");
#             bytes.extend_from_slice(&chunk[..count]);
#             assert!(bytes.len() <= 65_536, "request too large");
#         };
#         let size = {
#             let headers = std::str::from_utf8(&bytes[..header_end])?;
#             assert_eq!(headers.lines().next(), Some("POST /v1/chat/completions HTTP/1.1"));
#             let header = |name: &str| headers.lines().skip(1).find_map(|line| {
#                 let (key, value) = line.split_once(':')?;
#                 key.eq_ignore_ascii_case(name).then_some(value.trim())
#             });
#             assert_eq!(header("authorization"), Some("Bearer mock-key"));
#             assert_eq!(header("content-type"), Some("application/json"));
#             let size: usize = header("content-length").expect("body length").parse()?;
#             assert!(header_end + size <= 65_536, "request too large");
#             size
#         };
#         while bytes.len() < header_end + size {
#             let mut chunk = [0; 4096];
#             let count = socket.read(&mut chunk).await?;
#             assert!(count > 0, "request ended before body");
#             bytes.extend_from_slice(&chunk[..count]);
#             assert!(bytes.len() <= 65_536, "request too large");
#         }
#         let body: serde_json::Value = serde_json::from_slice(&bytes[header_end..header_end + size])?;
#         let mut messages = vec![
#             serde_json::json!({"role": "system", "content": "你是一个友好的助手"}),
#             serde_json::json!({"role": "user", "content": "我叫小明"}),
#         ];
#         if round == 1 {
#             messages.push(serde_json::json!({"role": "assistant", "content": REPLIES[0]}));
#             messages.push(serde_json::json!({"role": "user", "content": "我叫什么？"}));
#         }
#         assert_eq!(body, serde_json::json!({
#             "model": "mock-model", "messages": messages, "stream": false,
#             "max_tokens": 128, "temperature": 0.25,
#         }));
#         let response = serde_json::json!({
#             "id": format!("mock-{}", round + 1), "model": "mock-response-model",
#             "choices": [{"index": 0, "finish_reason": "stop", "message": {
#                 "role": "assistant", "content": REPLIES[round],
#             }}],
#             "usage": {"prompt_tokens": USAGE[round].0,
#                 "completion_tokens": USAGE[round].1, "total_tokens": USAGE[round].2},
#         }).to_string();
#         let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response);
#         socket.write_all(wire.as_bytes()).await?;
#         socket.shutdown().await?;
#     }
#     Ok(())
# }
#
# fn main() -> TestResult<()> {
#     tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
#         let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
#         let config = AiConfig {
#             api_key: "mock-key".into(),
#             base_url: format!("http://{}/v1", listener.local_addr()?),
#             model: "mock-model".into(), max_tokens: 128, temperature: 0.25,
#             ..AiConfig::default()
#         };
#         // Both futures are dropped on error or timeout, closing all mock sockets.
#         tokio::time::timeout(std::time::Duration::from_secs(10), async {
#             tokio::try_join!(mock(listener), async {
#                 chat_two_rounds(&config).await?;
#                 Ok::<(), Box<dyn std::error::Error>>(())
#             })
#         }).await??;
#         Ok(())
#     })
# }
```
