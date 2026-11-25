use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use crate::api::setup::error_response;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use nuono_core::auth::{CallerIdentity, Role};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::totp::own_account;
use super::{
    MAIL_SENDS_PER_MINUTE, account_security_error, mail_rate_key, provisioning_error,
    require_role_assignment, require_user_management,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::ChangeEmailRequest)]
pub struct ChangeEmailRequest {
    pub new_email: String,
    /// 当前密码。改邮箱等同于改账号找回入口，必须二次确认身份。
    #[schema(write_only, format = Password)]
    pub current_password: String,
}

/// POST /api/account/email
///
/// 发起更换邮箱：验证当前密码后，把确认码发到**新地址**。
///
/// 旧邮箱在确认前保持不变，因此中途放弃不会让账号失去找回入口。
#[utoipa::path(
    post,
    path = "/account/email",
    operation_id = "account_request_email_change",
    tag = "account",
    request_body = ChangeEmailRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn request_email_change(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChangeEmailRequest>,
) -> Response {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response.into_response(),
    };
    let email = body.new_email.trim().to_ascii_lowercase();
    if !state
        .rate_limiter
        .check(&mail_rate_key(&email), MAIL_SENDS_PER_MINUTE)
    {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "操作过于频繁，请稍后再试");
    }
    let mail = nuono_core::service::account_mail::AccountMailAdapter::new(
        state.db.clone(),
        state.config.snapshot().await,
    );
    match nuono_app::account_provisioning::AccountProvisioningService::new(&state.db)
        .request_email_change(&CallerIdentity::from_user(&user), &body.current_password, &email, &mail).await {
        Ok(()) => ApiResponse::ok(serde_json::json!({"message":"若该邮箱可用，确认码已发送到新地址，请在确认新邮箱页输入。确认后邮箱才会变更。"})).into_response(),
        Err(error) => provisioning_error(error),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::account::UserSummary)]
pub struct UserSummary {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type=String)]
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub role: String,
    pub status: String,
    /// 管理员是否已暂停该账号的 AI 使用权限（涵盖网页对话与所有已绑定聊天平台身份）。
    pub ai_blocked: bool,
    pub last_login_at: Option<String>,
    pub created_at: String,
}

/// GET /api/users
///
/// 账号列表。响应里没有 `password_hash`。
#[utoipa::path(
    get,
    path = "/users",
    operation_id = "account_list_users",
    tag = "account",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<UserSummary>>))
)]
pub(super) async fn list_users(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_user_management(&state, &headers).await {
        return denied.into_response();
    }
    let blocked_accounts = state.config.get().await.ai.blocked_accounts;
    match state.db.list_users().await {
        Ok(users) => ApiResponse::ok(
            users
                .into_iter()
                .map(|u| UserSummary {
                    ai_blocked: blocked_accounts.contains(&u.id),
                    id: u.id,
                    username: u.username,
                    email: u.email,
                    email_verified: u.email_verified,
                    role: u.role,
                    status: u.status,
                    last_login_at: u.last_login_at.map(nuono_core::clock::format_wire_ts),
                    created_at: nuono_core::clock::format_wire_ts(u.created_at),
                })
                .collect::<Vec<UserSummary>>(),
        )
        .into_response(),
        Err(e) => {
            warn!(error = %e, "读取账号列表失败");
            ApiResponse::<Vec<UserSummary>>::bad_request("无法读取账号列表，请稍后刷新页面")
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::CreateUserRequest)]
pub struct CreateUserRequest {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[schema(write_only, format = Password)]
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
}

/// POST /api/users
///
/// 由持有 `ManageUsers` 的调用者直接建号，建出的多数是 `user` 角色。
/// 关闭自助注册（`registration_enabled = false`）时这是唯一的加人途径，
/// 因此账号直接是 `active`，不需要邮件验证。
#[utoipa::path(
    post,
    path = "/users",
    operation_id = "account_create_user",
    tag = "account",
    request_body = CreateUserRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<UserSummary>))
)]
pub(super) async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    let actor = match require_user_management(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response.into_response(),
    };
    match nuono_app::account_provisioning::AccountProvisioningService::new(&state.db)
        .create(
            &CallerIdentity::from_user(&actor),
            &body.username,
            body.email.as_deref(),
            &body.password,
            body.role.as_deref(),
        )
        .await
    {
        Ok(user) => {
            let _ = nuono_core::audit::record_event(&state.db, "account.created_by_admin", None, Some(&actor.username),
                serde_json::json!({"username":user.username,"role":user.role,"actor":actor.username,"actor_role":actor.role})).await;
            ApiResponse::ok(UserSummary::from(user)).into_response()
        }
        Err(error) => account_security_error(error),
    }
}

/// DELETE /api/users/{id}
///
/// owner 专属（`AssignRoles`）。业务护栏在
/// [`nuono_core::service::users::delete_user`]：不能删自己、不能删最后一个 owner、
/// 名下还有 Bot/群归属时先转移或停用。
#[utoipa::path(
    delete,
    path = "/users/{id}",
    operation_id = "account_delete_user_handler",
    tag = "account",
    params(("id" = i64, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn delete_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> Response {
    let actor = match require_role_assignment(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };

    match nuono_app::accounts::AccountService::new(&state.db)
        .delete(
            &nuono_core::auth::CallerIdentity::from_user(&actor),
            user_id,
        )
        .await
    {
        Ok(deletion) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "account.deleted",
                None,
                Some(actor.username.as_str()),
                serde_json::json!({
                    "user_id": deletion.user_id.to_string(),
                    "username": deletion.username,
                    "revoked_sessions": deletion.revoked_sessions,
                    "actor": actor.username,
                }),
            )
            .await;
            info!(
                username = %deletion.username,
                revoked_sessions = deletion.revoked_sessions,
                actor = %actor.username,
                "🗑️ 账号已删除"
            );
            ApiResponse::ok(serde_json::json!({
                "message": format!("账号 {} 已删除", deletion.username),
            }))
            .into_response()
        }
        Err(e) => ApiResponse::<serde_json::Value>::from_err(&e).into_response(),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::SetRoleRequest)]
pub struct SetRoleRequest {
    pub role: String,
}

/// PATCH /api/users/{id}/role
///
/// 业务规则（禁自我降级、禁降最后一个 owner）全在
/// [`nuono_core::service::users::set_user_role`] 里，这里只做鉴权、解析和文案。
#[utoipa::path(
    patch,
    path = "/users/{id}/role",
    operation_id = "account_set_user_role_handler",
    tag = "account",
    request_body = SetRoleRequest,
    params(("id" = i64, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn set_user_role_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(target_id): Path<i64>,
    Json(body): Json<SetRoleRequest>,
) -> Response {
    let actor = match require_role_assignment(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };

    let Some(role) = Role::parse(body.role.trim()) else {
        return error_response(StatusCode::BAD_REQUEST, "只能选择所有者、管理员或普通成员");
    };

    match nuono_app::accounts::AccountService::new(&state.db)
        .set_role(
            &nuono_core::auth::CallerIdentity::from_user(&actor),
            target_id,
            role,
        )
        .await
    {
        Ok(outcome) => {
            // no-op 不写审计：把「点了但没变」也记下来会让审计流里全是噪音，
            // 真正的角色变更反而更难捞出来。
            let changed = outcome.previous != outcome.current;
            if changed {
                let _ = nuono_core::audit::record_event(
                    &state.db,
                    "account.role_changed",
                    None,
                    Some(actor.username.as_str()),
                    serde_json::json!({
                        "target_username": outcome.username,
                        "target_id": target_id.to_string(),
                        "from": outcome.previous.as_str(),
                        "to": outcome.current.as_str(),
                        "revoked_sessions": outcome.revoked_sessions,
                    }),
                )
                .await;
                info!(
                    target = %outcome.username,
                    from = %outcome.previous,
                    to = %outcome.current,
                    revoked_sessions = outcome.revoked_sessions,
                    actor = %actor.username,
                    "🔑 账号角色已变更"
                );
            }
            ApiResponse::ok(serde_json::json!({
                "username": outcome.username,
                "role": outcome.current.as_str(),
                "changed": changed,
                // 前端要据此提示「该账号已被强制下线」。
                "revoked_sessions": outcome.revoked_sessions,
            }))
            .into_response()
        }
        // 服务层的拒绝理由（「不能修改自己的角色」等）原样透出：它已经是给人看的中文，
        // 套一层「操作失败」只会盖掉唯一有用的信息。
        Err(e) => crate::api::ApiResponse::<serde_json::Value>::from_err(&e).into_response(),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::SetStatusRequest)]
pub struct SetStatusRequest {
    pub status: String,
}

/// PATCH /api/users/{id}/status
///
/// 停用 / 恢复账号。与改角色同级门槛（owner-only）：停用是人员退场的最后一道闸，
/// 交给 admin 意味着 admin 可以踢掉所有客户甚至 owner。
/// 业务规则（不能停自己、不能停最后一个 owner）在
/// [`nuono_core::service::users::set_user_status`] 里。
#[utoipa::path(
    patch,
    path = "/users/{id}/status",
    operation_id = "account_set_user_status_handler",
    tag = "account",
    request_body = SetStatusRequest,
    params(("id" = i64, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn set_user_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(target_id): Path<i64>,
    Json(body): Json<SetStatusRequest>,
) -> Response {
    let actor = match require_role_assignment(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };

    let status = body.status.trim();
    if status != nuono_core::db::STATUS_ACTIVE && status != nuono_core::db::STATUS_DISABLED {
        return error_response(StatusCode::BAD_REQUEST, "账号状态只能设为正常或停用");
    }

    match nuono_app::accounts::AccountService::new(&state.db)
        .set_status(
            &nuono_core::auth::CallerIdentity::from_user(&actor),
            target_id,
            status,
        )
        .await
    {
        Ok(outcome) => {
            let changed = outcome.previous != outcome.current;
            if changed {
                let _ = nuono_core::audit::record_event(
                    &state.db,
                    "account.status_changed",
                    None,
                    Some(actor.username.as_str()),
                    serde_json::json!({
                        "target_username": outcome.username,
                        "target_id": target_id.to_string(),
                        "from": outcome.previous,
                        "to": outcome.current,
                        "revoked_sessions": outcome.revoked_sessions,
                    }),
                )
                .await;
                info!(
                    target = %outcome.username,
                    from = %outcome.previous,
                    to = %outcome.current,
                    revoked_sessions = outcome.revoked_sessions,
                    actor = %actor.username,
                    "🔒 账号状态已变更"
                );
            }
            ApiResponse::ok(serde_json::json!({
                "username": outcome.username,
                "status": outcome.current,
                "changed": changed,
                "revoked_sessions": outcome.revoked_sessions,
            }))
            .into_response()
        }
        Err(e) => crate::api::ApiResponse::<serde_json::Value>::from_err(&e).into_response(),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::SetUserAiAccessRequest)]
pub struct SetUserAiAccessRequest {
    pub blocked: bool,
}

/// PATCH /api/users/{id}/ai-access
///
/// 暂停或恢复指定站点账号的 AI 使用权限（自动覆盖网页对话及其绑定的全部聊天平台身份）。
#[utoipa::path(
    patch,
    path = "/users/{id}/ai-access",
    operation_id = "account_set_user_ai_access",
    tag = "account",
    request_body = SetUserAiAccessRequest,
    params(("id" = i64, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn set_user_ai_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(target_id): Path<i64>,
    Json(body): Json<SetUserAiAccessRequest>,
) -> Response {
    let actor = match require_user_management(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    let target = match state.db.find_user_by_id(target_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "账号不存在"),
        Err(e) => return ApiResponse::<serde_json::Value>::from_err(&e).into_response(),
    };
    let mut blocked = state.config.get().await.ai.blocked_accounts;
    let was_blocked = blocked.contains(&target_id);
    if body.blocked {
        if !was_blocked {
            blocked.push(target_id);
        }
    } else {
        blocked.retain(|id| *id != target_id);
    }
    blocked.sort_unstable();
    blocked.dedup();
    if let Err(e) = state
        .config
        .update_patch(nuono_core::db::RuntimeConfigPatch {
            settings: serde_json::json!({ "ai": { "blocked_accounts": blocked } }),
            secrets: Vec::new(),
        })
        .await
    {
        return ApiResponse::<serde_json::Value>::from_err(&e).into_response();
    }
    if was_blocked != body.blocked {
        let _ = nuono_core::audit::record_event(
            &state.db,
            "account.ai_access_changed",
            None,
            Some(actor.username.as_str()),
            serde_json::json!({
                "target_id": target_id.to_string(),
                "target_username": target.username,
                "ai_blocked": body.blocked,
                "actor": actor.username,
            }),
        )
        .await;
    }
    ApiResponse::ok(serde_json::json!({
        "user_id": target_id.to_string(),
        "username": target.username,
        "ai_blocked": body.blocked,
    }))
    .into_response()
}
