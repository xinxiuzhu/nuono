use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};
use axum::{
    Extension,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use nuono_core::auth::{CallerIdentity, Capability};
use nuono_core::error::{NuonoError, Result};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{ApiReply, ApiResponse};
use crate::{
    AppState,
    sandbox::{CreateSession, MessageReceipt, SendMessage, SessionSnapshot},
};

async fn owner(state: &AppState, caller: Option<Extension<CallerIdentity>>) -> Result<i64> {
    session_owner(
        caller.as_ref().map(|Extension(caller)| caller),
        state.config.snapshot().await.security.admin_auth_enabled,
    )
}

fn session_owner(caller: Option<&CallerIdentity>, admin_auth_enabled: bool) -> Result<i64> {
    match caller {
        Some(caller) if caller.has(Capability::ManageRuntime) => Ok(caller.user_id),
        Some(_) => Err(NuonoError::PermissionDenied(
            "只有管理员可以使用测试对话".into(),
        )),
        // 遵循本机显式关闭管理员认证（admin_auth_enabled = false）时的免登录放行约定。
        None if !admin_auth_enabled => Ok(0),
        None => Err(NuonoError::Unauthorized("测试对话需要管理员登录".into())),
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    after: u64,
}

/// 管理员消息模拟沙盒：创建虚拟群聊/私聊会话以调试插件指令、按钮交互与事件管线。
#[utoipa::path(
    post, path = "/sessions", operation_id = "sandbox_create_session", tag = "sandbox",
    request_body = CreateSession,
    responses((status = 200, description = "创建独立虚拟会话", body = ApiResponse<SessionSnapshot>), (status = 403, description = "仅管理员"))
)]
async fn create_session(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Json(request): Json<CreateSession>,
) -> ApiReply<SessionSnapshot> {
    let result = match owner(&state, caller).await {
        Ok(owner) => state.sandbox.create(owner, request),
        Err(error) => Err(error),
    };
    match result {
        Ok(snapshot) => ApiResponse::ok(snapshot),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get, path = "/sessions/{session_id}", operation_id = "sandbox_get_session", tag = "sandbox",
    params(("session_id" = String, Path), ("after" = Option<u64>, Query, description = "仅返回 seq 大于该游标的消息")),
    responses((status = 200, description = "会话状态与新增消息", body = ApiResponse<SessionSnapshot>), (status = 403, description = "没有权限"))
)]
async fn get_session(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiReply<SessionSnapshot> {
    let result = match owner(&state, caller).await {
        Ok(owner) => state.sandbox.snapshot(owner, &id, query.after),
        Err(error) => Err(error),
    };
    match result {
        Ok(snapshot) => ApiResponse::ok(snapshot),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post, path = "/sessions/{session_id}/messages", operation_id = "sandbox_send_message", tag = "sandbox",
    params(("session_id" = String, Path)), request_body = SendMessage,
    responses((status = 200, description = "立即返回入站回执；处理和回复通过会话查询获取", body = ApiResponse<MessageReceipt>), (status = 403, description = "没有权限"))
)]
async fn send_message(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
    Json(request): Json<SendMessage>,
) -> ApiReply<MessageReceipt> {
    let result = match owner(&state, caller).await {
        Ok(owner) => state.sandbox.send(owner, &id, request),
        Err(error) => Err(error),
    };
    match result {
        Ok(receipt) => ApiResponse::ok(receipt),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    delete, path = "/sessions/{session_id}", operation_id = "sandbox_close_session", tag = "sandbox",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "关闭会话、取消处理中任务及虚拟发送路由；重复关闭也成功", body = ApiResponse<bool>), (status = 403, description = "没有权限"))
)]
async fn close_session(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
) -> ApiReply<bool> {
    let result = match owner(&state, caller).await {
        Ok(owner) => state.sandbox.delete(owner, &id),
        Err(error) => Err(error),
    };
    match result {
        Ok(closed) => ApiResponse::ok(closed),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get, path = "/sessions/{session_id}/media/{media_id}", operation_id = "sandbox_get_media", tag = "sandbox",
    params(("session_id" = String, Path), ("media_id" = String, Path)),
    responses((status = 200, description = "本会话已捕获的媒体字节", content_type = "application/octet-stream"), (status = 403, description = "没有权限"), (status = 404, description = "媒体已清理"))
)]
async fn get_media(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path((session_id, media_id)): Path<(String, String)>,
) -> Response {
    let result = match owner(&state, caller).await {
        Ok(owner) => state.sandbox.media(owner, &session_id, &media_id),
        Err(error) => Err(error),
    };
    media_response(result)
}

pub(super) fn media_response(result: Result<crate::sandbox::CapturedMedia>) -> Response {
    match result {
        Ok(media) => {
            let disposition = format!(
                "{}; filename*=UTF-8''{}",
                if media.inline { "inline" } else { "attachment" },
                urlencoding::encode(&media.file_name)
            );
            (
                [
                    (header::CONTENT_TYPE, media.content_type),
                    (header::CONTENT_DISPOSITION, disposition),
                    (header::CACHE_CONTROL, "no-store".into()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                ],
                media.bytes.to_vec(),
            )
                .into_response()
        }
        Err(error @ (NuonoError::PermissionDenied(_) | NuonoError::Unauthorized(_))) => {
            ApiResponse::<()>::from_err(&error).into_response()
        }
        Err(error) => (StatusCode::NOT_FOUND, ApiResponse::<()>::from_err(&error)).into_response(),
    }
}

pub(super) fn router() -> OpenApiRouter<AppState> {
    // 每次 `routes!` 调用都会把一组方法合并到同一个 MethodRouter，
    // 因此不同路径的 handler 必须分开调用 `.routes(...)` 注册。
    OpenApiRouter::new()
        .routes(routes!(create_session))
        .routes(routes!(get_session))
        .routes(routes!(send_message))
        .routes(routes!(close_session))
        .routes(routes!(get_media))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_core::auth::Role;

    #[test]
    fn sandbox_http_routes_construct_without_method_conflicts() {
        // 构造与生产启动完全一致的 API 路由树（无需初始化宿主状态、数据库连接或插件）。
        let _ = crate::api::router();

        let (_, document) = router().split_for_parts();
        let document = serde_json::to_value(document).unwrap();
        for (path, expected_methods) in [
            ("/sessions", vec!["post"]),
            ("/sessions/{session_id}", vec!["delete", "get"]),
            ("/sessions/{session_id}/messages", vec!["post"]),
            ("/sessions/{session_id}/media/{media_id}", vec!["get"]),
        ] {
            let mut methods: Vec<_> = document["paths"][path]
                .as_object()
                .unwrap_or_else(|| panic!("missing sandbox path: {path}"))
                .keys()
                .map(String::as_str)
                .collect();
            methods.sort_unstable();
            assert_eq!(methods, expected_methods, "unexpected methods at {path}");
        }
    }

    #[test]
    fn sandbox_uses_real_staff_ownership_and_the_explicit_local_auth_switch() {
        let admin = CallerIdentity::new(7, "admin", Role::Admin);
        let user = CallerIdentity::new(9, "user", Role::User);
        assert_eq!(session_owner(Some(&admin), true).unwrap(), 7);
        assert!(session_owner(Some(&user), true).is_err());
        assert!(session_owner(Some(&user), false).is_err());
        assert!(session_owner(None, true).is_err());
        assert_eq!(session_owner(None, false).unwrap(), 0);
        for (method, path) in [
            (axum::http::Method::POST, "/api/sandbox/sessions"),
            (
                axum::http::Method::POST,
                "/api/sandbox/sessions/id/messages",
            ),
            (axum::http::Method::GET, "/api/sandbox/sessions/id/media/id"),
            (axum::http::Method::DELETE, "/api/sandbox/sessions/id"),
        ] {
            assert_eq!(
                crate::middleware::route_capability(&method, path),
                Capability::ManageRuntime
            );
            assert!(!crate::middleware::is_public_api_request(&method, path));
        }
    }
}
