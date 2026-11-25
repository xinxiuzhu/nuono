use axum::{Json, Router, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

use crate::AppState;

mod account;
mod activity;
mod adapters;
mod ai;
mod ai_usage;
mod ai_web;
mod assistant;
mod audit;
pub mod auth;
mod bot_instances;
mod bot_qr_login;
mod bots;
mod chat;
mod commands;
mod companion;
#[cfg(test)]
#[path = "../../tests/unit/api/contract_tests.rs"]
pub(crate) mod contract_tests;
pub(crate) mod database;
mod database_settings;
mod docs;
mod group_plugins;
mod groups;
mod identity_assets;
mod input;
mod logs;
mod message_interactions_settings;
mod messages;
mod models;
mod notifications;
mod personal_plugins;
mod personas;
mod plugin_debug;
pub(crate) mod plugin_pages;
mod plugin_settings;
pub(crate) mod plugin_store;
mod plugins;
#[cfg(test)]
#[path = "../../tests/unit/api/result_tests.rs"]
mod result_tests;
mod role_media;
mod sandbox;
mod security;
mod service_policy;
mod sessions;
mod setup;
pub(crate) mod stats;
mod subscriptions;
mod system;
mod system_config;
mod system_control;
mod tasks;
pub(crate) mod theme;
mod upload;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiResponse<T: Serialize> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    /// 将服务层 `Result` 统一包装为带状态码与 JSON 信封的 HTTP 响应。
    pub fn from_result(result: nuono_core::Result<T>) -> ApiReply<T> {
        match result {
            Ok(data) => Self::ok(data),
            Err(error) => Self::from_err(&error),
        }
    }

    pub fn ok(data: T) -> ApiReply<T> {
        ApiReply {
            status: StatusCode::OK,
            body: Json(Self {
                success: true,
                data: Some(data),
                error: None,
            }),
        }
    }
    pub fn too_large(message: impl Into<String>) -> ApiReply<T> {
        ApiReply::error(StatusCode::PAYLOAD_TOO_LARGE, message)
    }
    pub fn not_found(message: impl Into<String>) -> ApiReply<T> {
        ApiReply::error(StatusCode::NOT_FOUND, message)
    }
    pub fn conflict(message: impl Into<String>) -> ApiReply<T> {
        ApiReply::error(StatusCode::CONFLICT, message)
    }
    pub fn forbidden(message: impl Into<String>) -> ApiReply<T> {
        ApiReply::error(StatusCode::FORBIDDEN, message)
    }
    pub fn unauthenticated(message: impl Into<String>) -> ApiReply<T> {
        ApiReply::error(StatusCode::UNAUTHORIZED, message)
    }
    pub fn internal(error: impl std::fmt::Display) -> ApiReply<T> {
        Self::from_err(&nuono_core::NuonoError::Internal(error.to_string()))
    }
    pub fn upstream(error: impl std::fmt::Display) -> ApiReply<T> {
        Self::from_err(&nuono_core::NuonoError::Http(error.to_string()))
    }
    pub fn timeout(error: impl std::fmt::Display) -> ApiReply<T> {
        Self::from_err(&nuono_core::NuonoError::Timeout(error.to_string()))
    }
    /// 将业务层错误映射为对应的 HTTP 状态码与统一 JSON 错误响应。
    /// 注意：业务层的 `Unauthorized` 代表权限不足（返回 403），只有会话认证中间件才会返回 401。
    pub fn from_err(err: &nuono_core::error::NuonoError) -> ApiReply<T> {
        use nuono_core::error::NuonoError as E;
        let status = match err {
            E::PermissionDenied(_) | E::PermissionValidation(_) | E::Unauthorized(_) => {
                StatusCode::FORBIDDEN
            }
            E::NotFound(_) | E::PluginNotFound(_) => StatusCode::NOT_FOUND,
            E::Conflict(_) => StatusCode::CONFLICT,
            E::Validation(_) | E::InvalidInput(_) | E::ConfigValidation(_) => {
                StatusCode::BAD_REQUEST
            }
            E::DatabaseConnection(_) | E::Overloaded(_) => StatusCode::SERVICE_UNAVAILABLE,
            E::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            E::Connection(_) | E::Http(_) | E::WebSocket(_) | E::OneBot(_) => {
                StatusCode::BAD_GATEWAY
            }
            E::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            if !matches!(err, E::Overloaded(_)) {
                tracing::error!(error = %err, kind = err.kind(), %status, "API operation failed");
            }
            let message = match status {
                StatusCode::SERVICE_UNAVAILABLE => "服务暂时不可用，请稍后重试",
                StatusCode::GATEWAY_TIMEOUT => "上游服务响应超时",
                StatusCode::BAD_GATEWAY => "上游服务请求失败",
                _ => "操作失败，请查看服务日志",
            };
            ApiReply::error(status, message)
        } else {
            ApiReply::error(status, err.to_string())
        }
    }
    pub fn bad_request(message: impl Into<String>) -> ApiReply<T> {
        ApiReply::error(StatusCode::BAD_REQUEST, message)
    }
}

/// 带显式状态码的响应。
///
/// 成功继续使用原信封；业务错误经 from_err 统一分类，错误入口不能生成成功状态码。
pub struct ApiReply<T: Serialize> {
    status: StatusCode,
    pub(crate) body: Json<ApiResponse<T>>,
}

impl<T: Serialize> ApiReply<T> {
    /// 为错误响应补充已发生副作用的上下文说明（避免直接暴露底层内部错误细节）。
    pub fn context(mut self, context: &str) -> Self {
        if let Some(error) = &mut self.body.0.error {
            *error = format!("{context}；{error}");
        }
        self
    }

    /// 转换响应的数据载荷类型（常用于鉴权辅助函数返回的错误响应转换为当前 handler 的返回类型），保留原失败状态码与错误信息。
    pub fn cast<U: Serialize>(self) -> ApiReply<U> {
        ApiReply::error(
            self.status,
            self.body.0.error.unwrap_or_else(|| "操作失败".into()),
        )
    }

    /// 构造错误响应，确保 HTTP 状态码一定是 4xx/5xx 且 JSON 载荷中 `success` 为 `false`。
    pub fn error(status: StatusCode, message: impl Into<String>) -> Self {
        let (status, message) = if status.is_client_error() || status.is_server_error() {
            (status, message.into())
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "错误响应使用了无效状态码".to_string(),
            )
        };
        Self {
            status,
            body: Json(ApiResponse {
                success: false,
                data: None,
                error: Some(message),
            }),
        }
    }
}

impl<T: Serialize> IntoResponse for ApiReply<T> {
    fn into_response(self) -> axum::response::Response {
        (self.status, self.body).into_response()
    }
}

impl<T: Serialize> From<Json<ApiResponse<T>>> for ApiReply<T> {
    fn from(body: Json<ApiResponse<T>>) -> Self {
        if body.0.success {
            Self {
                status: StatusCode::OK,
                body,
            }
        } else {
            Self::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "操作失败，请查看服务日志",
            )
        }
    }
}

pub fn router() -> Router<AppState> {
    let (router, document) = documented_router().split_for_parts();
    router
        .merge(docs::router(docs::configure(document)))
        .method_not_allowed_fallback(method_not_allowed)
}

pub(crate) async fn not_found() -> ApiReply<()> {
    ApiResponse::not_found("API 路由不存在")
}
async fn method_not_allowed() -> ApiReply<()> {
    ApiReply::error(StatusCode::METHOD_NOT_ALLOWED, "该接口不支持此请求方法")
}

fn documented_router() -> utoipa_axum::router::OpenApiRouter<AppState> {
    utoipa_axum::router::OpenApiRouter::new()
        .routes(utoipa_axum::routes!(stats::get_stats))
        .routes(utoipa_axum::routes!(activity::statistics))
        .routes(utoipa_axum::routes!(stats::get_trend))
        .routes(utoipa_axum::routes!(stats::get_command_stats))
        .routes(utoipa_axum::routes!(stats::get_health))
        .routes(utoipa_axum::routes!(bots::get_bots))
        .routes(utoipa_axum::routes!(bots::delete_bot_handler))
        .nest("/api", bot_instances::router())
        .nest("/api", bot_qr_login::router())
        .nest("/api/bots", bots::router())
        .nest("/api/commands", commands::router())
        .nest("/api/logs", logs::router())
        .routes(utoipa_axum::routes!(plugins::list_services))
        .routes(utoipa_axum::routes!(plugins::list_triggers))
        .nest("/api/plugins", plugins::router())
        .merge(plugin_pages::router())
        .nest("/api/config", system_config::router())
        .nest("/api/system", system::router().merge(service_policy::router()).merge(system_control::router()))
        .nest("/api", plugin_store::router())
        .nest("/api/database", database::router())
        .nest("/api", theme::router())
        .nest("/api", auth::router())
        .nest("/api", setup::router())
        .nest("/api", account::router())
        .nest("/api", identity_assets::router())
        .nest("/api/subscriptions", subscriptions::router())
        .nest("/api/tasks", tasks::router())
        .nest("/api", assistant::router())
        // 按群的插件设置 merge 进同一个 nest：`/api/groups` 只能 nest 一次，
        // 重复 nest 同前缀会在启动时 panic。
        .nest(
            "/api/groups",
            groups::router().merge(group_plugins::router()),
        )
        .nest("/api/messages", messages::router())
        .nest("/api/notifications", notifications::router())
        .nest("/api/security", security::router())
        .nest("/api/audit", audit::router())
        .nest("/api/plugin-debug", plugin_debug::router())
        .nest("/api/sandbox", sandbox::router())
        .nest("/api/chat", chat::router())
        .nest("/api/ai", ai::router().merge(ai_web::router()).merge(ai_usage::router()))
        .nest("/api/ai", personas::router())
        .nest("/api/models", models::router())
        .nest("/api", upload::router())
        .nest("/api", upload::public_router())
        .nest("/api", adapters::router())
        .nest("/api/sessions", sessions::router())
}

#[cfg(test)]
mod tests {
    use super::{ApiReply, ApiResponse};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use nuono_core::error::NuonoError;

    fn status_of(err: NuonoError) -> StatusCode {
        ApiResponse::<String>::from_err(&err)
            .into_response()
            .status()
    }

    /// 授权类拒绝必须是 403，不能是 200。
    ///
    /// 这一条是从真实误判里长出来的：`ensure_group_serviceable` 拒绝时曾经走 旧失败信封入口，
    /// HTTP 状态码是 200，只有包体里 `success:false`。我照状态码读结论，就把「已拦住」
    /// 误报成了「没拦住」。状态码退回 200 的话这个用例会红。
    #[test]
    fn authorization_denials_map_to_forbidden() {
        assert_eq!(
            status_of(NuonoError::PermissionDenied("当前无法管理该群".into())),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_of(NuonoError::PermissionValidation("不是你的群".into())),
            StatusCode::FORBIDDEN
        );
    }

    /// `Unauthorized` 也给 403 而不是 401。
    ///
    /// 401 会让前端拦截器直接跳登录页。服务层这个变体表达的是「这个账号不能碰这个东西」，
    /// 把它映射成 401 会让在期用户碰到一次越权就被踢出登录态。
    #[test]
    fn service_level_unauthorized_does_not_bounce_to_login() {
        assert_eq!(
            status_of(NuonoError::Unauthorized("不允许修改该字段".into())),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn typed_errors_use_http_failure_statuses() {
        for (error, expected) in [
            (
                NuonoError::ConfigValidation("字段缺失".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                NuonoError::Validation("输入无效".into()),
                StatusCode::BAD_REQUEST,
            ),
            (NuonoError::NotFound("model".into()), StatusCode::NOT_FOUND),
            (NuonoError::Conflict("in use".into()), StatusCode::CONFLICT),
            (
                NuonoError::Overloaded("password workers full".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                NuonoError::DatabaseQuery("SQL with credentials".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                NuonoError::DatabaseConnection("offline".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                NuonoError::Timeout("provider".into()),
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                NuonoError::OneBot("Bot offline".into()),
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            assert_eq!(status_of(error), expected);
        }
    }
    #[tokio::test]
    async fn internal_details_are_logged_but_not_returned_to_clients() {
        let response = ApiResponse::<()>::from_err(&NuonoError::DatabaseQuery(
            "SELECT secret-key-value".into(),
        ))
        .into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["success"], false);
        assert!(body["data"].is_null());
        assert!(!body.to_string().contains("secret-key-value"));
    }

    #[tokio::test]
    async fn compact_error_reply_preserves_http_status_and_json() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let response = ApiReply::<()>::error(status, "拒绝访问").into_response();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["content-type"], "application/json");
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                serde_json::json!({"success": false, "data": null, "error": "拒绝访问"})
            );
        }
        assert!(
            std::mem::size_of::<ApiReply<()>>() < std::mem::size_of::<axum::response::Response>()
        );
    }
}
