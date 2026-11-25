//! 首次启动的账号初始化向导。
//!
//! 数据库中还没有任何用户时，`/api/setup` 允许无认证创建第一个 owner 账号；创建成功后
//! 该端点永久关闭（`user_count() > 0` 即拒绝），因此它是一次性的，不存在长期开放的注册口。
//!
//! 安全边界：向导开放期间，任何能访问监听端口的人都可以抢先创建 owner。默认监听
//! `127.0.0.1`，把 host 改成 `0.0.0.0` 是一个知情动作——公网暴露前应先完成初始化。

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use nuono_core::db::{NewUser, ROLE_OWNER, STATUS_ACTIVE};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(setup_status))
        .routes(routes!(create_first_user))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::setup::SetupStatus)]
pub struct SetupStatus {
    /// 是否仍需初始化。false 表示已有账号，向导已关闭。
    pub needs_setup: bool,
    /// 管理服务可用与业务运行是独立状态；外部连接不影响此状态。
    pub business_state: crate::business_startup::BusinessPhase,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::setup::CreateFirstUserRequest)]
pub struct CreateFirstUserRequest {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[schema(write_only, format = Password)]
    pub password: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::setup::CreateFirstUserResponse)]
pub struct CreateFirstUserResponse {
    pub username: String,
    pub role: String,
}

/// GET /api/setup/status
///
/// 前端据此决定是显示初始化向导还是登录页。
#[utoipa::path(
    get,
    path = "/setup/status",
    operation_id = "setup_setup_status",
    tag = "setup",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<SetupStatus>))
)]
async fn setup_status(State(state): State<AppState>) -> crate::api::ApiReply<SetupStatus> {
    match state.db.user_count().await {
        Ok(count) => ApiResponse::ok(SetupStatus {
            needs_setup: count == 0,
            business_state: state.business_startup.phase(),
        }),
        Err(e) => {
            tracing::error!(error = %e, "读取账号状态失败");
            ApiResponse::internal("无法读取账号状态，请稍后刷新页面")
        }
    }
}

/// POST /api/setup
///
/// 创建第一个 owner 账号。已存在任何用户时返回 409，保证只能成功一次。
#[utoipa::path(
    post,
    path = "/setup",
    operation_id = "setup_create_first_user",
    tag = "setup",
    request_body = CreateFirstUserRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<CreateFirstUserResponse>), (status = 409, description = "已存在账号，初始化向导永久关闭", body = ApiResponse<super::docs::Empty>))
)]
async fn create_first_user(
    State(state): State<AppState>,
    Json(body): Json<CreateFirstUserRequest>,
) -> Response {
    // 关键不变式：仅在零用户时可用。这一检查同时承担「向导只能用一次」的职责。
    match state.db.user_count().await {
        Ok(0) => {}
        Ok(_) => {
            metrics::counter!("nuono_setup_rejected_total", "reason" => "already_initialized")
                .increment(1);
            return error_response(
                StatusCode::CONFLICT,
                "系统已完成初始化，不能重复执行。如需添加账号，请登录后在账号管理中操作。",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "初始化向导读取账号状态失败");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "无法读取账号状态，请稍后刷新页面",
            );
        }
    }

    let username = body.username.trim().to_string();
    if let Err(message) = validate_username(&username) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    if let Err(message) = validate_password_strength(&body.password) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }

    // 邮箱是可选的：本地部署可以不填，后续邮件注册功能需要时再补。
    let email = match body
        .email
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(value) => match validate_email(value) {
            Ok(()) => Some(value.to_string()),
            Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
        },
        None => None,
    };

    let created = state
        .db
        // 原子入口：advisory lock + 事务内复查计数，上面的预检查只是快速路径。
        // 并发向导请求只会有一个成功，另一个在此拿到 None。
        .create_first_owner(NewUser {
            username: username.clone(),
            email,
            password: body.password,
            role: ROLE_OWNER.to_string(),
            status: STATUS_ACTIVE.to_string(),
            // 首启向导：操作者当场录入自己的邮箱，没有验证信环节。留 false 会让 owner
            // 永久用不了「忘记密码」。
            email_verified: true,
        })
        .await;

    match created {
        Ok(Some(user)) => {
            state.business_startup.setup_changed();
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.first_user_created",
                None,
                // 向导执行时库里还没有账号，主体在 data 里。
                None,
                serde_json::json!({"username": user.username, "role": user.role}),
            )
            .await;
            tracing::info!(
                username = %user.username,
                "🔐 首个 owner 账号已创建，初始化向导已关闭"
            );
            ApiResponse::ok(CreateFirstUserResponse {
                username: user.username,
                role: user.role,
            })
            .into_response()
        }
        // 锁内复查发现已有账号：并发向导里另一个请求赢了，语义与预检查命中相同。
        Ok(None) => {
            metrics::counter!("nuono_setup_rejected_total", "reason" => "already_initialized")
                .increment(1);
            error_response(
                StatusCode::CONFLICT,
                "系统已完成初始化，不能重复执行。如需添加账号，请登录后在账号管理中操作。",
            )
        }
        Err(error @ nuono_core::NuonoError::Overloaded(_)) => {
            ApiResponse::<CreateFirstUserResponse>::from_err(&error).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "初始化向导创建账号失败");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "账号创建失败，请稍后重试",
            )
        }
    }
}

pub(crate) use nuono_app::auth::{validate_email, validate_password_strength, validate_username};

pub(crate) fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::ApiReply::<()>::error(status, message).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_rules_reject_short_and_invalid_chars() {
        assert!(validate_username("ab").is_err());
        assert!(validate_username("valid_user-1.x").is_ok());
        assert!(validate_username("bad space").is_err());
        assert!(validate_username("bad/slash").is_err());
        assert!(validate_username(&"a".repeat(65)).is_err());
    }

    #[test]
    fn email_rules_reject_structurally_invalid_input() {
        assert!(validate_email("user@example.com").is_ok());
        assert!(validate_email("no-at-sign").is_err());
        assert!(validate_email("two@at@signs.com").is_err());
        assert!(validate_email("user@nodot").is_err());
        assert!(validate_email("user@.leading.dot").is_err());
        assert!(validate_email("has space@example.com").is_err());
    }
}
