//! 无状态的 OpenAI Responses 协议传输实现，由 Nuono 自行维护对话历史与工具执行流程。
use super::*;
use nuono_ai::UsageInfo;

impl OpenAiCompatibleProvider {
    fn responses_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/responses") {
            base.to_owned()
        } else {
            format!("{base}/responses")
        }
    }

    fn responses_body(&self, request: &ChatRequest, stream: bool) -> Result<Value, AiError> {
        validate_request_images(request)?;
        let model = request.model.as_deref().unwrap_or(&self.default_model);
        let mut input = Vec::new();
        for message in &request.messages {
            // 加密的推理状态仅在原接口地址与原模型完全一致时才回传。
            for item in &message.continuation {
                if item["endpoint"] == self.responses_url() && item["model"] == model {
                    input.push(item["item"].clone());
                }
            }
            match message.role {
                MessageRole::System => continue,
                MessageRole::Tool => {
                    let output = if message.images.is_empty() {
                        Value::String(message.content.clone())
                    } else {
                        let mut parts =
                            vec![serde_json::json!({"type":"input_text", "text":message.content})];
                        parts.extend(message.images.iter().map(|source| {
                            serde_json::json!({"type":"input_image", "image_url":source, "detail":"auto"})
                        }));
                        Value::Array(parts)
                    };
                    input.push(serde_json::json!({"type":"function_call_output", "call_id":message.tool_call_id, "output":output}));
                }
                _ => {
                    if !message.images.is_empty() {
                        let mut content = Vec::new();
                        if !message.content.is_empty() {
                            content.push(
                                serde_json::json!({"type": "input_text", "text": message.content}),
                            );
                        }
                        content.extend(message.images.iter().map(|url| {
                            serde_json::json!({"type": "input_image", "image_url": url, "detail": "auto"})
                        }));
                        input.push(serde_json::json!({"role": message.role, "content": content}));
                    } else if !message.content.is_empty() {
                        input.push(
                            serde_json::json!({"role":message.role, "content":message.content}),
                        );
                    }
                    for call in &message.tool_calls {
                        input.push(serde_json::json!({"type":"function_call", "call_id":call.id,
                            "name":provider_tool_name(&call.name), "arguments":call.arguments.to_string()}));
                    }
                }
            }
        }
        let mut body = serde_json::json!({"model":model, "input":input, "stream":stream,
            "store":false, "include":["reasoning.encrypted_content"],
            "max_output_tokens":request.max_tokens.unwrap_or(self.max_tokens)});
        // 并非所有 Responses 推理模型都支持采样参数（如 temperature），
        // 因此保留服务商默认值，不强行带入 Chat Completions 的默认采样参数。
        if let Some(prompt) = build_preamble(request, &self.system_prompt) {
            body["instructions"] = Value::String(prompt);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(|tool| serde_json::json!({
                "type":"function", "name":provider_tool_name(&tool.name),
                "description":format!("{}\nNuono host tool name: {}", tool.description, tool.name),
                "parameters":tool.parameters_schema, "strict":false
            })).collect());
        }
        Ok(body)
    }

    pub(super) async fn responses_chat(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse, AiError> {
        let aliases = tool_aliases(&request)?;
        let model = request
            .model
            .as_deref()
            .unwrap_or(&self.default_model)
            .to_owned();
        let body = self.responses_body(&request, false)?;
        let mut attempt = crate::usage::Attempt::new(
            self.usage_tracking.clone(),
            &model,
            nuono_ai::usage::UsageOperation::Chat,
        );
        let result = async {
            let response = self
                .client
                .post(self.responses_url())
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(request_error)?;
            let status = response.status();
            let text = super::read_bounded_response_text(response, status.is_success()).await?;
            let value = serde_json::from_str::<Value>(&text);
            if let Ok(value) = &value {
                attempt.report(crate::usage::responses_usage(value));
            }
            if !status.is_success() {
                return Err(provider_error(status.as_u16(), &text));
            }
            let value = value.map_err(|e| AiError::ResponseParseFailed(e.to_string()))?;
            parse_response(&value, &model, &self.responses_url(), &aliases)
        }
        .await;
        attempt.finish(if result.is_ok() {
            nuono_ai::usage::UsageOutcome::Succeeded
        } else {
            nuono_ai::usage::UsageOutcome::Failed
        });
        result
    }

    pub(super) async fn responses_stream(
        &self,
        request: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChatStreamChunk, AiError>>, AiError> {
        tool_aliases(&request)?;
        let model = request
            .model
            .as_deref()
            .unwrap_or(&self.default_model)
            .to_owned();
        let body = self.responses_body(&request, true)?;
        let mut attempt = crate::usage::Attempt::new(
            self.usage_tracking.clone(),
            &model,
            nuono_ai::usage::UsageOperation::ChatStream,
        );
        let response = async {
            let response = self
                .client
                .post(self.responses_url())
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(request_error)?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let text = super::read_bounded_response_text(response, false).await?;
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    attempt.report(crate::usage::responses_usage(&value));
                }
                return Err(provider_error(status, &text));
            }
            Ok(response)
        }
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                attempt.finish(nuono_ai::usage::UsageOutcome::Failed);
                return Err(error);
            }
        };
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        tokio::spawn(async move {
            let outcome = sse::forward(
                response,
                tx,
                |data| {
                    if data.trim() == "[DONE]" {
                        return Ok(sse::Event::Ignore);
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(data) {
                        attempt.report(crate::usage::responses_usage(&value["response"]));
                    }
                    parse_event(data, &model)
                        .map(|chunk| chunk.map(sse::Event::Chunk).unwrap_or(sse::Event::Ignore))
                },
                "Responses 流提前结束",
                false,
            )
            .await;
            attempt.finish(outcome);
        });
        Ok(rx)
    }
}

fn parse_response(
    value: &Value,
    model: &str,
    endpoint: &str,
    aliases: &HashMap<String, String>,
) -> Result<ChatResponse, AiError> {
    if value["status"] != "completed" {
        return Err(AiError::RequestFailed(format!(
            "Responses 未完成：{}",
            value
                .get("error")
                .filter(|v| !v.is_null())
                .or(value.get("incomplete_details"))
                .unwrap_or(&value["status"])
        )));
    }
    let output = value["output"]
        .as_array()
        .ok_or_else(|| AiError::ResponseParseFailed("Responses 缺少 output".into()))?;
    let mut content = String::new();
    let mut calls = Vec::new();
    let mut continuation = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                if let Some(parts) = item["content"].as_array() {
                    for part in parts {
                        content.push_str(
                            part["text"]
                                .as_str()
                                .or(part["refusal"].as_str())
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            Some("function_call") => {
                let name = item["name"].as_str().ok_or_else(|| {
                    AiError::ResponseParseFailed("function_call 缺少 name".into())
                })?;
                let id = item["call_id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        AiError::ResponseParseFailed("function_call 缺少 call_id".into())
                    })?;
                let arguments = serde_json::from_str(item["arguments"].as_str().unwrap_or(""))
                    .map_err(|e| AiError::ResponseParseFailed(format!("工具参数无效：{e}")))?;
                calls.push(ChatToolCall {
                    id: Some(id.into()),
                    name: aliases.get(name).cloned().unwrap_or_else(|| name.into()),
                    arguments,
                });
            }
            Some("reasoning") if item["encrypted_content"].is_string() => {
                continuation
                    .push(serde_json::json!({"endpoint":endpoint,"model":model,"item":item}));
            }
            _ => {}
        }
    }
    let usage = &value["usage"];
    Ok(ChatResponse {
        id: value["id"].as_str().unwrap_or_default().into(),
        model: value["model"].as_str().unwrap_or(model).into(),
        content,
        finish_reason: Some(
            if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .into(),
        ),
        tool_calls: calls,
        continuation,
        usage: UsageInfo {
            prompt_tokens: usage["input_tokens"]
                .as_u64()
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
            completion_tokens: usage["output_tokens"]
                .as_u64()
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
            total_tokens: usage["total_tokens"]
                .as_u64()
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
        },
    })
}

fn parse_event(data: &str, model: &str) -> Result<Option<ChatStreamChunk>, AiError> {
    let event: Value =
        serde_json::from_str(data).map_err(|e| AiError::ResponseParseFailed(e.to_string()))?;
    let kind = event["type"].as_str().unwrap_or_default();
    if kind == "response.completed"
        && event["response"]
            .get("status")
            .is_some_and(|status| status != "completed")
    {
        return Err(AiError::RequestFailed(format!(
            "Responses 流未完成：{event}"
        )));
    }
    match kind {
        "response.failed" | "response.incomplete" | "error" => Err(AiError::RequestFailed(
            format!("Responses 流失败：{}", event),
        )),
        "response.output_text.delta" | "response.refusal.delta" | "response.completed" => {
            Ok(Some(ChatStreamChunk {
                id: event["response"]["id"].as_str().unwrap_or_default().into(),
                model: event["response"]["model"].as_str().unwrap_or(model).into(),
                delta_content: event["delta"].as_str().unwrap_or_default().into(),
                finish_reason: (kind == "response.completed").then(|| "stop".into()),
            }))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_ai::ChatTool;
    use serde_json::json;

    #[test]
    fn responses_tool_images_are_real_function_output_parts() {
        let provider = OpenAiCompatibleProvider::new(
            "test",
            "https://example.com/v1",
            ProviderType::Responses,
        )
        .unwrap();
        let image = "data:image/png;base64,iVBORw0KGgo=";
        let request = ChatRequest::new(vec![
            nuono_ai::ChatMessage::tool_result("visual-call", "typed result")
                .with_images(vec![image.into()]),
            nuono_ai::ChatMessage::tool_result("legacy-call", "legacy result"),
        ]);
        for streaming in [false, true] {
            let body = provider.responses_body(&request, streaming).unwrap();
            assert_eq!(
                body["input"][0],
                json!({
                    "type":"function_call_output", "call_id":"visual-call", "output":[
                        {"type":"input_text", "text":"typed result"},
                        {"type":"input_image", "image_url":image, "detail":"auto"},
                    ],
                })
            );
            assert_eq!(
                body["input"][1],
                json!({"type":"function_call_output", "call_id":"legacy-call", "output":"legacy result"})
            );
            assert_eq!(body["input"].as_array().unwrap().len(), 2);
        }
    }
    #[test]
    fn roundtrip_tools_and_encrypted_reasoning_without_server_history() {
        let provider =
            OpenAiCompatibleProvider::new("key", "https://example.com/v1", ProviderType::Responses)
                .unwrap();
        let request = ChatRequest::new(vec![
            nuono_ai::ChatMessage::user("查一下")
                .with_images(vec!["https://example.com/image.png".into()]),
        ])
        .with_model("reasoner")
        .with_tools(vec![ChatTool::new(
            "plugin.search",
            "搜索",
            json!({"type":"object"}),
        )]);
        let wire_name = provider_tool_name("plugin.search");
        let response = parse_response(&json!({"status":"completed", "output":[
            {"type":"reasoning", "id":"rs_1", "summary":[], "encrypted_content":"opaque"},
            {"type":"function_call","call_id":"call_1","name":wire_name,"arguments":"{\"q\":\"你好\"}"}
        ],"usage":{"input_tokens":5,"output_tokens":6,"total_tokens":11}}), "reasoner", &provider.responses_url(), &tool_aliases(&request).unwrap()).unwrap();
        assert_eq!(response.tool_calls[0].name, "plugin.search");
        assert_eq!(response.usage.total_tokens, 11);
        let mut followup = request;
        followup.messages.push(
            nuono_ai::ChatMessage::assistant("")
                .with_tool_calls(response.tool_calls)
                .with_continuation(response.continuation),
        );
        followup
            .messages
            .push(nuono_ai::ChatMessage::tool_result("call_1", "结果"));
        let body = provider.responses_body(&followup, false).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(
            body["input"][0]["content"],
            json!([
                {"type":"input_text", "text":"查一下"},
                {"type":"input_image", "image_url":"https://example.com/image.png", "detail":"auto"},
            ])
        );
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][2]["name"], wire_name);
        assert_eq!(body["input"][3]["call_id"], "call_1");
        followup.model = Some("another".into());
        assert_eq!(
            provider.responses_body(&followup, false).unwrap()["input"][1]["type"],
            "function_call"
        );
    }
    #[tokio::test]
    async fn transport_uses_responses_endpoint_and_preserves_fragmented_unicode() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for streaming in [false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                };
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                assert!(headers.starts_with("POST /v1/responses "));
                let size = headers
                    .lines()
                    .find_map(|line| {
                        let (name, val) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| val.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                while bytes.len() < end + size {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let request: Value = serde_json::from_slice(&bytes[end..end + size]).unwrap();
                assert_eq!(request["stream"], streaming);
                assert_eq!(request["store"], false);
                assert_eq!(request["input"][0]["content"], "你好");
                assert_eq!(
                    request["input"][1]["content"],
                    json!([
                        {"type":"input_image", "image_url":"https://example.com/image.png", "detail":"auto"},
                        {"type":"input_image", "image_url":"data:image/png;base64,iVBORw0KGgo=", "detail":"auto"},
                    ])
                );
                let (content_type, body) = if streaming {
                    ("text/event-stream", concat!(
                        "event: response.output_text.delta\n",
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"m\"}}\n\n"
                    ).to_owned())
                } else {
                    ("application/json", json!({"id":"r","status":"completed","model":"m","output":[{"type":"message","content":[{"type":"output_text","text":"你好"}]}]}).to_string())
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                for byte in body.as_bytes() {
                    socket.write_all(&[*byte]).await.unwrap();
                    if streaming {
                        tokio::task::yield_now().await;
                    }
                }
            }
        });
        let provider =
            OpenAiCompatibleProvider::new("test-key", &base, ProviderType::Responses).unwrap();
        let request = ChatRequest::new(vec![
            nuono_ai::ChatMessage::user("你好"),
            nuono_ai::ChatMessage::user("").with_images(vec![
                "https://example.com/image.png".into(),
                "data:image/png;base64,iVBORw0KGgo=".into(),
            ]),
        ]);
        assert_eq!(
            provider.chat(request.clone()).await.unwrap().content,
            "你好"
        );
        let mut stream = provider.chat_stream(request).await.unwrap();
        let mut text = String::new();
        let mut finished = false;
        while let Some(chunk) = stream.recv().await {
            let chunk = chunk.unwrap();
            text.push_str(&chunk.delta_content);
            finished |= chunk.finish_reason.is_some();
        }
        assert_eq!(text, "你好");
        assert!(finished);
        server.await.unwrap();
    }

    #[test]
    fn failures_and_stream_completion_are_explicit() {
        assert!(
            parse_response(&json!({"status":"incomplete"}), "m", "url", &HashMap::new()).is_err()
        );
        assert!(parse_event(r#"{"type":"response.failed"}"#, "m").is_err());
        assert_eq!(
            parse_event(
                r#"{"type":"response.output_text.delta","delta":"你好"}"#,
                "m"
            )
            .unwrap()
            .unwrap()
            .delta_content,
            "你好"
        );
        assert_eq!(
            parse_event(
                r#"{"type":"response.completed","response":{"id":"r","model":"m"}}"#,
                "m"
            )
            .unwrap()
            .unwrap()
            .finish_reason
            .as_deref(),
            Some("stop")
        );
    }
}
