//! AI 对话、流式 SSE 与会话历史管理接口。

use axum::{
    extract::State,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::{Deserialize, Serialize};

use super::private_scope;
use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use nuono_core::ai::{AiService, ChatMessage, ChatRequest};
use nuono_core::service::ai_models::build_chat_provider;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::ai::ChatPayload)]
pub struct ChatPayload {
    pub messages: Vec<ChatMessagePayload>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub model_id: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, Clone, utoipa::ToSchema)]
#[schema(as = api::ai::ChatMessagePayload)]
pub struct ChatMessagePayload {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::ChatResponsePayload)]
pub struct ChatResponsePayload {
    pub content: String,
    pub model: String,
}

/// 管理员直连 LLM 调试对话接口（不经过插件指令管线；普通用户网页对话请使用 `/api/chat`，个人助手请使用 `/api/assistant`）。
#[utoipa::path(
    post,
    path = "/chat",
    operation_id = "ai_chat",
    tag = "ai",
    request_body = ChatPayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ChatResponsePayload>))
)]
pub(super) async fn chat(
    State(state): State<AppState>,
    caller: Option<axum::Extension<crate::middleware::CallerIdentity>>,
    Json(body): Json<ChatPayload>,
) -> crate::api::ApiReply<ChatResponsePayload> {
    if body
        .session_id
        .as_deref()
        .is_some_and(|id| id.starts_with(nuono_app::agent::ACCOUNT_CHAT_PREFIX))
    {
        return ApiResponse::bad_request("该会话标识由宿主管理");
    }
    let _conversation = state
        .manager
        .event_kernel
        .agent_runtime
        .begin_conversation();
    let (provider, model_name) = match build_chat_provider(
        &state.db,
        &state.config,
        body.model_id,
        body.model.as_deref(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return ApiResponse::from_err(&e),
    };

    let messages: Vec<ChatMessage> = body
        .messages
        .iter()
        .map(|m| match m.role.as_str() {
            "system" => ChatMessage::system(m.content.clone()),
            "assistant" => ChatMessage::assistant(m.content.clone()),
            _ => ChatMessage::user(m.content.clone()),
        })
        .collect();

    let requested_model = model_name.clone();
    let provider_name = provider.service_name().to_owned();
    let req = ChatRequest::new(messages).with_model(model_name);

    let timeout = state
        .config
        .snapshot()
        .await
        .ai
        .provider_timeout_secs
        .clamp(1, 300);
    let started_at_ms = nuono_core::clock::db_now().timestamp_millis();
    let started_instant = std::time::Instant::now();
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(timeout), provider.chat(req)).await;
    let finished_at_ms = nuono_core::clock::db_now().timestamp_millis();
    let latency_ms = started_instant.elapsed().as_millis().min(i32::MAX as u128) as i32;
    let user_id = caller.map(|ext| ext.0.user_id);

    let last_user = body
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str());

    match response {
        Ok(Ok(resp)) => {
            if let Some(ref session_id) = body.session_id {
                let _ = persist_web_chat_run(
                    &state.db,
                    WebChatRunRecord {
                        session_id,
                        user_id,
                        user_content: last_user,
                        assistant_content: Some(resp.content.as_str()),
                        provider: &provider_name,
                        model: &resp.model,
                        status: nuono_core::conversation_store::AI_RUN_STATUS_SUCCEEDED,
                        prompt_tokens: resp.usage.prompt_tokens.min(i32::MAX as u32) as i32,
                        completion_tokens: resp.usage.completion_tokens.min(i32::MAX as u32) as i32,
                        latency_ms,
                        error_message: None,
                        created_at_ms: started_at_ms,
                        finished_at_ms,
                    },
                )
                .await;
            }
            ApiResponse::ok(ChatResponsePayload {
                content: resp.content,
                model: resp.model,
            })
        }
        Ok(Err(nuono_core::ai::AiError::Timeout(error))) => {
            if let Some(ref session_id) = body.session_id {
                let _ = persist_web_chat_run(
                    &state.db,
                    WebChatRunRecord {
                        session_id,
                        user_id,
                        user_content: last_user,
                        assistant_content: None,
                        provider: &provider_name,
                        model: &requested_model,
                        status: nuono_core::conversation_store::AI_RUN_STATUS_FAILED,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms,
                        error_message: Some(error.as_str()),
                        created_at_ms: started_at_ms,
                        finished_at_ms,
                    },
                )
                .await;
            }
            ApiResponse::timeout(error)
        }
        Ok(Err(e)) => {
            let msg = format!("AI 请求失败: {}", e);
            if let Some(ref session_id) = body.session_id {
                let _ = persist_web_chat_run(
                    &state.db,
                    WebChatRunRecord {
                        session_id,
                        user_id,
                        user_content: last_user,
                        assistant_content: None,
                        provider: &provider_name,
                        model: &requested_model,
                        status: nuono_core::conversation_store::AI_RUN_STATUS_FAILED,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms,
                        error_message: Some(msg.as_str()),
                        created_at_ms: started_at_ms,
                        finished_at_ms,
                    },
                )
                .await;
            }
            ApiResponse::upstream(msg)
        }
        Err(_) => {
            let msg = format!("模型请求超过 {timeout} 秒，请检查连接或调整超时");
            if let Some(ref session_id) = body.session_id {
                let _ = persist_web_chat_run(
                    &state.db,
                    WebChatRunRecord {
                        session_id,
                        user_id,
                        user_content: last_user,
                        assistant_content: None,
                        provider: &provider_name,
                        model: &requested_model,
                        status: nuono_core::conversation_store::AI_RUN_STATUS_FAILED,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms,
                        error_message: Some(msg.as_str()),
                        created_at_ms: started_at_ms,
                        finished_at_ms,
                    },
                )
                .await;
            }
            ApiResponse::timeout(msg)
        }
    }
}

struct WebChatRunRecord<'a> {
    session_id: &'a str,
    user_id: Option<i64>,
    user_content: Option<&'a str>,
    assistant_content: Option<&'a str>,
    provider: &'a str,
    model: &'a str,
    status: &'a str,
    prompt_tokens: i32,
    completion_tokens: i32,
    latency_ms: i32,
    error_message: Option<&'a str>,
    created_at_ms: i64,
    finished_at_ms: i64,
}

async fn persist_web_chat_run(
    db: &nuono_core::Database,
    record: WebChatRunRecord<'_>,
) -> nuono_core::Result<()> {
    use nuono_core::conversation_store::{
        CompletedAiRun, DIRECTION_INBOUND, DIRECTION_OUTBOUND, EnsureConversation, NewMessage,
        PLATFORM_WEB, ROLE_ASSISTANT, ROLE_USER, SCOPE_WEB, append_message_on,
        ensure_active_conversation_on, record_completed_ai_run_on,
    };
    let tx = db.begin_write().await?;
    let conv = ensure_active_conversation_on(
        &tx,
        EnsureConversation {
            platform: PLATFORM_WEB.to_owned(),
            bot_instance_id: None,
            scope_type: SCOPE_WEB.to_owned(),
            group_id: None,
            owner_user_id: record.user_id,
            native_chat_id: record.session_id.to_owned(),
            native_thread_id: String::new(),
            title: None,
        },
    )
    .await?;
    let trigger_message_id = match record.user_content {
        Some(text) => {
            append_message_on(
                &tx,
                NewMessage {
                    conversation_id: Some(conv.id.clone()),
                    platform: PLATFORM_WEB.to_owned(),
                    bot_instance_id: None,
                    native_message_id: None,
                    reply_to_native_message_id: None,
                    sender_user_id: conv.owner_user_id,
                    sender_identity_id: None,
                    native_sender_id: None,
                    native_group_id: None,
                    sender_nickname: None,
                    direction: DIRECTION_INBOUND.to_owned(),
                    role: ROLE_USER.to_owned(),
                    text_content: text.to_owned(),
                    content_blocks_json: "[]".to_owned(),
                    raw_payload_json: None,
                    occurred_at_ms: record.created_at_ms,
                },
            )
            .await?
        }
        None => None,
    };
    let reply_message_id = match record.assistant_content {
        Some(text) => {
            append_message_on(
                &tx,
                NewMessage {
                    conversation_id: Some(conv.id.clone()),
                    platform: PLATFORM_WEB.to_owned(),
                    bot_instance_id: None,
                    native_message_id: None,
                    reply_to_native_message_id: None,
                    sender_user_id: None,
                    sender_identity_id: None,
                    native_sender_id: None,
                    native_group_id: None,
                    sender_nickname: None,
                    direction: DIRECTION_OUTBOUND.to_owned(),
                    role: ROLE_ASSISTANT.to_owned(),
                    text_content: text.to_owned(),
                    content_blocks_json: "[]".to_owned(),
                    raw_payload_json: None,
                    occurred_at_ms: record.finished_at_ms,
                },
            )
            .await?
        }
        None => None,
    };
    record_completed_ai_run_on(
        &tx,
        CompletedAiRun {
            id: None,
            conversation_id: conv.id,
            trigger_message_id,
            reply_message_id,
            user_id: conv.owner_user_id,
            provider: record.provider.to_owned(),
            model: record.model.to_owned(),
            status: record.status.to_owned(),
            prompt_tokens: record.prompt_tokens,
            completion_tokens: record.completion_tokens,
            latency_ms: record.latency_ms,
            steps_json: "[]".to_owned(),
            error_message: record.error_message.map(str::to_owned),
            created_at_ms: record.created_at_ms,
            finished_at_ms: Some(record.finished_at_ms),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[utoipa::path(
    post,
    path = "/chat/stream",
    operation_id = "ai_chat_stream",
    tag = "ai",
    request_body = ChatPayload,
    responses((status = 200, description = "SSE 流；data 为 JSON 分片（delta_content、finish_reason 等）或 error 对象，流结束时关闭连接", body = String, content_type = "text/event-stream"))
)]
pub(super) async fn chat_stream(
    State(state): State<AppState>,
    Json(body): Json<ChatPayload>,
) -> axum::response::Response {
    let conversation = state
        .manager
        .event_kernel
        .agent_runtime
        .begin_conversation();
    let (provider, model_name) = match build_chat_provider(
        &state.db,
        &state.config,
        body.model_id,
        body.model.as_deref(),
    )
    .await
    {
        Ok(provider) => provider,
        Err(error) => return ApiResponse::<serde_json::Value>::from_err(&error).into_response(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, !>>(64);

    tokio::spawn(async move {
        let _conversation = conversation;

        let messages: Vec<ChatMessage> = body
            .messages
            .iter()
            .map(|m| match m.role.as_str() {
                "system" => ChatMessage::system(m.content.clone()),
                "assistant" => ChatMessage::assistant(m.content.clone()),
                _ => ChatMessage::user(m.content.clone()),
            })
            .collect();

        let req = ChatRequest::new(messages).with_model(model_name);

        let mut stream_rx = match provider.chat_stream(req).await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(error=%e,"AI stream failed");
                let event = Event::default()
                    .data(serde_json::json!({"error": "上游模型请求失败"}).to_string());
                let _ = tx.send(Ok(event)).await;
                return;
            }
        };

        while let Some(result) = stream_rx.recv().await {
            match result {
                Ok(chunk) => {
                    let data = serde_json::to_string(&chunk).unwrap_or_default();
                    if tx.send(Ok(Event::default().data(data))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error=%e,"AI stream failed");
                    let data = serde_json::json!({"error": "上游模型流中断"}).to_string();
                    let _ = tx.send(Ok(Event::default().data(data))).await;
                    break;
                }
            }
        }
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::ConversationMessage)]
pub struct ConversationMessage {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::ConversationSession)]
pub struct ConversationSession {
    pub session_id: String,
    pub messages: Vec<ConversationMessage>,
}

#[utoipa::path(
    get,
    path = "/conversations",
    operation_id = "ai_list_conversations",
    tag = "ai",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<ConversationSession>>))
)]
pub(super) async fn list_conversations(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<ConversationSession>> {
    let models = match state.db.list_ai_conversations().await {
        Ok(m) => m,
        Err(e) => return ApiResponse::from_err(&e),
    };

    let mut sessions: std::collections::HashMap<String, Vec<ConversationMessage>> =
        std::collections::HashMap::new();

    for m in models.into_iter().filter(|m| !private_scope(&m.session_id)) {
        let (role, content) = present_conversation_content(m.role, m.content);
        sessions
            .entry(m.session_id)
            .or_default()
            .push(ConversationMessage {
                id: m.id,
                role,
                content,
                timestamp: m.timestamp.map(nuono_core::clock::format_wire_ts),
            });
    }

    let result: Vec<ConversationSession> = sessions
        .into_iter()
        .map(|(session_id, messages)| ConversationSession {
            session_id,
            messages,
        })
        .collect();

    ApiResponse::ok(result)
}

pub(super) fn present_conversation_content(role: String, content: String) -> (String, String) {
    use nuono_app::agent::input::{HistoryMessage, USER_CONTEXT_ROLE};
    if role != USER_CONTEXT_ROLE {
        return (role, content);
    }
    let content = HistoryMessage::decode(&role, &content)
        .map(|message| message.chat_message().content)
        .unwrap_or(content);
    ("user".into(), content)
}

#[utoipa::path(
    delete,
    path = "/conversations/{session}",
    operation_id = "ai_delete_conversation",
    tag = "ai",
    params(("session" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::Empty>))
)]
pub(super) async fn delete_conversation(
    State(state): State<AppState>,
    Path(session): Path<String>,
) -> crate::api::ApiReply<()> {
    if private_scope(&session) {
        return ApiResponse::not_found("对话不存在");
    }
    match state.db.delete_ai_conversation_session(&session).await {
        Ok(_) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}
