//! 账号级网页对话接口：复用与沙盒测试模式相同的指令、插件及 AI 处理管线。
use axum::{extract::State, response::Response};
use nuono_core::{
    auth::Capability,
    error::{NuonoError, Result},
};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    ApiReply, ApiResponse,
    input::{ApiCaller, ApiJson, ApiPath, ApiQuery},
};
use crate::{
    AppState,
    sandbox::{MessageReceipt, SendMessage, SessionSnapshot},
};

/// 会话身份完全由当前已认证的登录账号派生，不接受客户端自报身份。
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct OpenConversation {}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    after: u64,
}

fn owner(caller: &nuono_core::auth::CallerIdentity) -> Result<i64> {
    if caller.has(Capability::ManageOwnAccount) {
        Ok(caller.user_id)
    } else {
        Err(NuonoError::PermissionDenied(
            "当前账号无法使用网页对话".into(),
        ))
    }
}

/// 打开当前登录账号的网页对话会话（走完整插件指令 + AI 管线，支持在网页端直接调用业务插件）。
#[utoipa::path(post, path="/sessions", operation_id="chat_open_session", tag="chat",
    request_body=OpenConversation, responses((status=200, body=ApiResponse<SessionSnapshot>)))]
async fn open(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiJson(_request): ApiJson<OpenConversation>,
) -> ApiReply<SessionSnapshot> {
    ApiResponse::from_result(
        owner(&caller).and_then(|id| state.sandbox.conversation(id, &caller.username)),
    )
}

#[utoipa::path(get, path="/sessions/{id}", operation_id="chat_read_session", tag="chat",
    params(("id"=String,Path),("after"=Option<u64>,Query)),
    responses((status=200, body=ApiResponse<SessionSnapshot>)))]
async fn read(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> ApiReply<SessionSnapshot> {
    ApiResponse::from_result(owner(&caller).and_then(|owner| {
        state.sandbox.require_conversation(owner, &id)?;
        state.sandbox.snapshot(owner, &id, query.after)
    }))
}

#[utoipa::path(post, path="/sessions/{id}/messages", operation_id="chat_send_message", tag="chat",
    params(("id"=String,Path)), request_body=SendMessage,
    responses((status=200, body=ApiResponse<MessageReceipt>)))]
async fn send(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(id): ApiPath<String>,
    ApiJson(message): ApiJson<SendMessage>,
) -> ApiReply<MessageReceipt> {
    ApiResponse::from_result(owner(&caller).and_then(|owner| {
        state.sandbox.require_conversation(owner, &id)?;
        state.sandbox.send(owner, &id, message)
    }))
}

#[utoipa::path(delete, path="/sessions/{id}", operation_id="chat_close_session", tag="chat",
    params(("id"=String,Path)), responses((status=200, body=ApiResponse<bool>)))]
async fn close(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(id): ApiPath<String>,
) -> ApiReply<bool> {
    ApiResponse::from_result(
        owner(&caller).and_then(|owner| state.sandbox.close_conversation(owner, &id)),
    )
}

#[utoipa::path(get, path="/sessions/{id}/media/{media_id}", operation_id="chat_read_media", tag="chat",
    params(("id"=String,Path),("media_id"=String,Path)), responses((status=200, content_type="application/octet-stream")))]
async fn media(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath((id, media_id)): ApiPath<(String, String)>,
) -> Response {
    super::sandbox::media_response(owner(&caller).and_then(|owner| {
        state.sandbox.require_conversation(owner, &id)?;
        state.sandbox.media(owner, &id, &media_id)
    }))
}

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(open))
        .routes(routes!(read))
        .routes(routes!(send))
        .routes(routes!(close))
        .routes(routes!(media))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn account_chat_cannot_select_a_test_identity() {
        for body in [
            serde_json::json!({"owner": 9}),
            serde_json::json!({"test_mode": true}),
            serde_json::json!({"user_id": 9}),
            serde_json::json!({"group_role": "owner"}),
            serde_json::json!({"message_type": "group"}),
        ] {
            assert!(serde_json::from_value::<OpenConversation>(body).is_err());
        }
        assert!(serde_json::from_value::<OpenConversation>(serde_json::json!({})).is_ok());
    }

    #[test]
    fn only_registered_chat_operations_are_account_accessible() {
        let (_, spec) = router().split_for_parts();
        assert_eq!(spec.paths.paths.len(), 4);
        for (method, path) in [
            (Method::POST, "/api/chat/sessions"),
            (Method::GET, "/api/chat/sessions/a"),
            (Method::DELETE, "/api/chat/sessions/a"),
            (Method::POST, "/api/chat/sessions/a/messages"),
            (Method::GET, "/api/chat/sessions/a/media/b"),
        ] {
            assert_eq!(
                crate::middleware::route_capability(&method, path),
                Capability::ManageOwnAccount
            );
            assert!(!crate::middleware::is_public_api_request(&method, path));
        }
        for path in [
            "/api/sandbox/sessions",
            "/api/chat/sessions/a/test",
            "/api/chat/sessions/a/messages/extra",
            "/api/chat/sessions/",
        ] {
            assert_eq!(
                crate::middleware::route_capability(&Method::POST, path),
                Capability::ManageRuntime
            );
        }
    }
}
