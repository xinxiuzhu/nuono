//! 账号与身份管理 HTTP 接口：统一校验凭证、已验证平台绑定、会话管理及事务级账号合并。
//! 匿名找回邮箱统一返回中性响应以防枚举；特权写操作始终从当前登录态解析操作人身份。

mod binding_merge;
mod identities;
mod public;
mod totp;
mod users;

use crate::AppState;
use crate::api::setup::error_response;
use crate::api::{ApiReply, ApiResponse};
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use nuono_app::account_security::{AccountSecurityService, TokenOperation};
use nuono_core::auth::{CallerIdentity, Capability};
use nuono_core::entity::users as user_entity;
use tracing::warn;
use utoipa_axum::{router::OpenApiRouter, routes};

#[cfg(test)]
use public::{RegisterRequest, VerifyRegistrationOtpRequest};
use users::UserSummary;
#[cfg(test)]
use users::{ChangeEmailRequest, CreateUserRequest};

use binding_merge::{
    __path_account_merge_history, __path_account_merge_status, __path_account_sessions,
    __path_begin_platform_binding, __path_binding_bots, __path_cancel_account_merge,
    __path_cancel_platform_binding, __path_confirm_account_merge, __path_confirm_platform_binding,
    __path_platform_binding_status, __path_platform_identities, __path_prepare_account_merge,
    __path_revoke_account_session, __path_unbind_platform_identity, account_merge_history,
    account_merge_status, account_sessions, begin_platform_binding, binding_bots,
    cancel_account_merge, cancel_platform_binding, confirm_account_merge, confirm_platform_binding,
    platform_binding_status, platform_identities, prepare_account_merge, revoke_account_session,
    unbind_platform_identity,
};
use identities::{
    __path_admin_bind_user_identity, __path_admin_unbind_user_identity,
    __path_list_user_identities, admin_bind_user_identity, admin_unbind_user_identity,
    list_user_identities,
};
#[cfg(test)]
use public::duplicate_email_response;
use public::{
    __path_confirm_email_change, __path_forgot_password, __path_register,
    __path_registration_status, __path_resend_verification, __path_reset_password,
    __path_verify_email, __path_verify_registration_otp, confirm_email_change, forgot_password,
    register, registration_status, resend_verification, reset_password, verify_email,
    verify_registration_otp,
};
use totp::{
    __path_totp_confirm, __path_totp_disable, __path_totp_enroll, __path_totp_regenerate_recovery,
    totp_confirm, totp_disable, totp_enroll, totp_regenerate_recovery,
};
use users::{
    __path_create_user, __path_delete_user_handler, __path_list_users, __path_request_email_change,
    __path_set_user_ai_access_handler, __path_set_user_role_handler,
    __path_set_user_status_handler, create_user, delete_user_handler, list_users,
    request_email_change, set_user_ai_access_handler, set_user_role_handler,
    set_user_status_handler,
};

/// 同一邮箱每分钟允许触发的发信次数。
///
/// 取 2 而不是 1：用户在弱网下重复点击「重发」是常见操作，留一次容错；但足以挡住
/// 拿接口当发信机的滥用。
const MAIL_SENDS_PER_MINUTE: u32 = 2;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        // 公开：无认证可访问，靠令牌或配置开关自身约束。
        .routes(routes!(register))
        .routes(routes!(registration_status))
        .routes(routes!(verify_email))
        .routes(routes!(verify_registration_otp))
        .routes(routes!(resend_verification))
        .routes(routes!(forgot_password))
        .routes(routes!(reset_password))
        .routes(routes!(confirm_email_change))
        // 需认证：由 security_middleware 统一拦截。
        .routes(routes!(request_email_change))
        .routes(routes!(prepare_account_merge, account_merge_history))
        .routes(routes!(account_merge_status, cancel_account_merge))
        .routes(routes!(confirm_account_merge))
        .routes(routes!(account_sessions))
        .routes(routes!(revoke_account_session))
        .routes(routes!(binding_bots))
        .routes(routes!(begin_platform_binding))
        .routes(routes!(platform_binding_status, confirm_platform_binding, cancel_platform_binding))
        .routes(routes!(platform_identities))
        .routes(routes!(unbind_platform_identity))
        .routes(routes!(list_users, create_user))
        .routes(routes!(list_user_identities))
        .routes(routes!(admin_bind_user_identity))
        .routes(routes!(admin_unbind_user_identity))
        .routes(routes!(delete_user_handler))
        // 用 PATCH 而不是 PUT：只改单个状态字段，PUT 语义上要求整体替换。
        .routes(routes!(set_user_role_handler))
        .routes(routes!(set_user_status_handler))
        .routes(routes!(set_user_ai_access_handler))
        // 两步验证：只碰自己的账号（能力门槛见 middleware）。
        .routes(routes!(totp_enroll))
        .routes(routes!(totp_confirm))
        .routes(routes!(totp_disable))
        .routes(routes!(totp_regenerate_recovery))
}

/// 按邮箱构造限流键。
///
/// 与 IP 限流共用 [`crate::middleware::RateLimiter`]，加 `mail:` 前缀避免与 IP 键冲突。
fn mail_rate_key(email: &str) -> String {
    format!("mail:{email}")
}

/// 从请求头解析当前登录用户名。
/// 要求当前会话持有账号管理能力，否则返回可直接下发的错误响应。
///
/// 这是 `security_middleware` 之外的第二道关，两道都在：中间件按路由判能力，这里再解析出
/// 操作者本身 —— 后续的建号、改角色、审计都要知道「是谁在操作」，光有布尔准入不够。
///
/// 停用账号（`status != active`）一并拒绝：否则停用只挡登录，已签发的会话还能继续建号。
async fn require_user_management(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<user_entity::Model, ApiReply<()>> {
    let Some(token) = crate::api::auth::extract_token_from_headers(headers) else {
        return Err(ApiReply::error(StatusCode::UNAUTHORIZED, "请先登录"));
    };

    // 401 和 403 必须分开：前端见 401 才跳登录页。合成一个的话，会话过期的管理员会卡在
    // 「无权访问」提示上，而正确的动作是重新登录。
    let Some(user) = crate::api::auth::authenticated_user_checked(&state.db, &token)
        .await
        .map_err(|error| ApiResponse::from_err(&error))?
    else {
        // 会话无效、账号已删除或已停用都走这条，不泄露账号是否存在。
        return Err(ApiReply::error(StatusCode::UNAUTHORIZED, "请先登录"));
    };

    let caller = CallerIdentity::from_user(&user);
    if !caller.has(Capability::ManageUsers) {
        warn!(
            username = %caller.username,
            role = %caller.role,
            "拒绝越权访问账号管理接口"
        );
        return Err(ApiReply::error(
            StatusCode::FORBIDDEN,
            Capability::ManageUsers.denial_message(),
        ));
    }
    Ok(user)
}

/// 改角色专用闸门：比 [`require_user_management`] 严一档，要求 `AssignRoles`（只有 owner 持有）。
///
/// 不复用 `ManageUsers` 的原因和 `POST /api/users` 里拒绝 admin 建 admin 一样 —— admin 能改角色
/// 等于 admin 能把自己或同伙提成 owner，`ManageUsers` 就成了通往最高权限的一步之遥。
async fn require_role_assignment(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<user_entity::Model, ApiReply<()>> {
    let user = require_user_management(state, headers).await?;
    let caller = CallerIdentity::from_user(&user);
    if !caller.has(Capability::AssignRoles) {
        warn!(
            username = %caller.username,
            role = %caller.role,
            "拒绝：只有 owner 能变更账号角色"
        );
        return Err(ApiReply::error(
            StatusCode::FORBIDDEN,
            Capability::AssignRoles.denial_message(),
        ));
    }
    Ok(user)
}

async fn redeem_token_response(
    state: &AppState,
    token: &str,
    operation: TokenOperation,
    event: &str,
    invalid: &str,
    message: &str,
) -> Response {
    match AccountSecurityService::new(&state.db)
        .redeem(token, operation)
        .await
    {
        Ok(Some(result)) => {
            let _=nuono_core::audit::record_event(&state.db,event,None,None,
                serde_json::json!({"user_id":result.user_id.to_string(),"revoked_sessions":result.revoked_sessions})).await;
            ApiResponse::ok(serde_json::json!({"message":message})).into_response()
        }
        Ok(None) => error_response(StatusCode::BAD_REQUEST, invalid),
        Err(error) => account_security_error(error),
    }
}
fn account_security_error(error: nuono_core::NuonoError) -> Response {
    ApiResponse::<()>::from_err(&error).into_response()
}

impl From<nuono_app::account_provisioning::ManagedAccount> for UserSummary {
    fn from(user: nuono_app::account_provisioning::ManagedAccount) -> Self {
        Self {
            id: user.id,
            username: user.username,
            email: user.email,
            email_verified: user.email_verified,
            role: user.role,
            status: user.status,
            ai_blocked: false,
            last_login_at: user.last_login_at,
            created_at: user.created_at,
        }
    }
}
fn provisioning_error(error: nuono_app::account_provisioning::ProvisioningError) -> Response {
    use nuono_app::account_provisioning::ProvisioningError;
    match error {
        ProvisioningError::Unavailable => {
            error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
        ProvisioningError::Delivery(message) => {
            ApiResponse::<()>::upstream(message).into_response()
        }
        ProvisioningError::Account(error) => account_security_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_rate_key_is_namespaced_to_avoid_ip_collision() {
        // 限流器按字符串键计数，IP 与邮箱共用一张表；前缀确保
        // 邮箱 "127.0.0.1" 这类输入不会消耗某个 IP 的额度。
        assert_eq!(mail_rate_key("a@b.com"), "mail:a@b.com");
        assert_ne!(mail_rate_key("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn duplicate_email_response_is_a_conflict_without_account_details() {
        let response = duplicate_email_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn register_payload_requires_all_three_fields() {
        // 注册三要素缺一不可：缺字段应在反序列化阶段失败，而不是落到业务里当空串处理。
        assert!(
            serde_json::from_value::<RegisterRequest>(serde_json::json!({
                "username": "alice", "email": "a@b.com", "password": "long-enough-pass"
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<RegisterRequest>(serde_json::json!({
                "username": "alice", "email": "a@b.com"
            }))
            .is_err(),
            "缺少 password 应被拒绝"
        );
    }

    #[test]
    fn registration_otp_payload_requires_email_and_eight_digit_code() {
        assert!(
            serde_json::from_value::<VerifyRegistrationOtpRequest>(serde_json::json!({
                "email": "alice@example.com", "code": "00123456"
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<VerifyRegistrationOtpRequest>(serde_json::json!({
                "email": "alice@example.com"
            }))
            .is_err()
        );
    }

    #[test]
    fn create_user_payload_defaults_optional_fields() {
        let minimal: CreateUserRequest = serde_json::from_value(serde_json::json!({
            "username": "bob", "password": "long-enough-pass"
        }))
        .unwrap();
        assert_eq!(minimal.email, None);
        assert_eq!(minimal.role, None);
    }

    #[test]
    fn change_email_payload_requires_current_password() {
        // 改邮箱必须带当前密码：会话可能被盗用，不能只凭 cookie 就改掉找回入口。
        assert!(
            serde_json::from_value::<ChangeEmailRequest>(serde_json::json!({
                "new_email": "new@example.com"
            }))
            .is_err(),
            "缺少 current_password 应被拒绝"
        );
    }
}
