use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use crate::api::setup::{error_response, validate_email};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use nuono_app::account_security::TokenOperation;
use nuono_core::db::{REGISTRATION_OTP_LENGTH, RegistrationOtpVerification};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{MAIL_SENDS_PER_MINUTE, account_security_error, mail_rate_key, redeem_token_response};

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::account::RegistrationStatus)]
pub struct RegistrationStatus {
    /// 是否允许自助注册。false 时关闭自助注册，仅管理员可建号，注册入口应隐藏。
    pub registration_enabled: bool,
    /// 邮件功能是否可用。false 时验证、找回密码均不可用。
    pub mail_enabled: bool,
}

/// GET /api/auth/registration-status
///
/// 前端据此决定是否显示「注册」和「忘记密码」入口。
#[utoipa::path(
    get,
    path = "/auth/registration-status",
    operation_id = "account_registration_status",
    tag = "account",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<RegistrationStatus>))
)]
pub(super) async fn registration_status(
    State(state): State<AppState>,
) -> crate::api::ApiReply<RegistrationStatus> {
    let cfg = state.config.snapshot().await;
    // 用 is_ready 而非 cfg.mail.enabled：开关开着但 SMTP 凭据还没录入时，入口就该继续隐藏。
    let mail_ready = nuono_core::service::mail::is_ready(&cfg.mail);
    ApiResponse::ok(RegistrationStatus {
        registration_enabled: cfg.mail.registration_enabled && mail_ready,
        mail_enabled: mail_ready,
    })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::RegisterRequest)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    #[schema(write_only, format = Password)]
    pub password: String,
}

/// POST /api/auth/register
///
/// 保存临时注册请求并发送验证码，邮箱验证成功后才分配正式账号 ID。
/// 重复提交会使旧验证码失效，并沿用同一邮箱的发送频率配额。
#[utoipa::path(
    post,
    path = "/auth/register",
    operation_id = "account_register",
    tag = "account",
    request_body = RegisterRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Response {
    let email = body.email.trim().to_ascii_lowercase();
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
    match nuono_app::account_registration::RegistrationService::new(&state.db, &mail)
        .register(&body.username, &email, &body.password)
        .await
    {
        Ok(receipt) => {
            let event = if receipt.created {
                "account.registration_requested"
            } else {
                "account.registration_retried"
            };
            let _ = nuono_core::audit::record_event(
                &state.db,
                event,
                None,
                None,
                serde_json::json!({"request_id":receipt.request_id}),
            )
            .await;
            registration_accepted_response()
        }
        Err(error) => {
            use nuono_app::account_registration::RegistrationError as E;
            match error {
                E::Unavailable => {
                    error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
                }
                E::Disabled => error_response(StatusCode::FORBIDDEN, error.to_string()),
                E::UsernameTaken => error_response(StatusCode::CONFLICT, error.to_string()),
                E::EmailTaken => duplicate_email_response(),
                E::Other(error) => account_security_error(error),
            }
        }
    }
}

pub(super) fn duplicate_email_response() -> Response {
    error_response(
        StatusCode::CONFLICT,
        nuono_app::account_registration::RegistrationError::EmailTaken.to_string(),
    )
}

pub(super) fn registration_accepted_response() -> Response {
    ApiResponse::ok(serde_json::json!({
        "message": "注册请求已受理，邮箱验证码已发送，请查收后完成验证。"
    }))
    .into_response()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::TokenRequest)]
pub struct TokenRequest {
    pub token: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::VerifyRegistrationOtpRequest)]
pub struct VerifyRegistrationOtpRequest {
    pub email: String,
    pub code: String,
}

/// POST /api/auth/verify-email
///
/// 在同一数据库事务中核销邮箱校验凭证并确认邮箱归属。
#[utoipa::path(
    post,
    path = "/auth/verify-email",
    operation_id = "account_verify_email",
    tag = "account",
    request_body = TokenRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn verify_email(
    State(state): State<AppState>,
    Json(body): Json<TokenRequest>,
) -> Response {
    redeem_token_response(
        &state,
        &body.token,
        TokenOperation::VerifyEmail,
        "account.email_verified",
        "验证码无效或已过期",
        "邮箱验证成功，现在可以登录了。",
    )
    .await
}

/// POST /api/auth/verify-registration-otp
///
/// 验证用户手动输入的注册验证码。失败提示统一，防止通过错误差异探测账号或验证码状态。
#[utoipa::path(
    post,
    path = "/auth/verify-registration-otp",
    operation_id = "account_verify_registration_otp",
    tag = "account",
    request_body = VerifyRegistrationOtpRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn verify_registration_otp(
    State(state): State<AppState>,
    Json(body): Json<VerifyRegistrationOtpRequest>,
) -> Response {
    let email = body.email.trim().to_ascii_lowercase();
    if validate_email(&email).is_err()
        || body.code.len() != REGISTRATION_OTP_LENGTH
        || !body.code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return error_response(StatusCode::BAD_REQUEST, "验证码无效或已过期");
    }

    let cfg = state.config.snapshot().await;
    let outcome = match state
        .db
        .verify_registration_otp(&email, &body.code, &cfg.security.registration_otp_pepper)
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            warn!(error = %e, "验证注册邮箱验证码失败");
            return account_security_error(e);
        }
    };

    match outcome {
        RegistrationOtpVerification::Verified { user_id } => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "account.registration_otp_verified",
                None,
                None,
                serde_json::json!({"user_id": user_id.to_string()}),
            )
            .await;
            info!(user_id, "🔓 注册邮箱验证码验证通过，账号已激活");
            ApiResponse::ok(serde_json::json!({"message": "邮箱验证成功，现在可以登录了。"}))
                .into_response()
        }
        RegistrationOtpVerification::Invalid => {
            error_response(StatusCode::BAD_REQUEST, "验证码无效或已过期")
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::EmailRequest)]
pub struct EmailRequest {
    pub email: String,
}

/// POST /api/auth/resend-verification
///
/// 重发验证邮件。响应不区分邮箱是否存在或是否已验证。
#[utoipa::path(
    post,
    path = "/auth/resend-verification",
    operation_id = "account_resend_verification",
    tag = "account",
    request_body = EmailRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn resend_verification(
    State(state): State<AppState>,
    Json(body): Json<EmailRequest>,
) -> Response {
    let mail = nuono_core::service::account_mail::AccountMailAdapter::new(
        state.db.clone(),
        state.config.snapshot().await,
    );
    if !nuono_app::account_registration::AccountMail::ready(&mail) {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "邮件功能尚未就绪，请联系管理员。",
        );
    }
    let email = body.email.trim().to_ascii_lowercase();
    if let Err(message) = validate_email(&email) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    if !state
        .rate_limiter
        .check(&mail_rate_key(&email), MAIL_SENDS_PER_MINUTE)
    {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "操作过于频繁，请稍后再试");
    }
    match nuono_app::account_registration::RegistrationService::new(&state.db, &mail)
        .resend_verification(&email)
        .await
    {
        Ok(()) => ApiResponse::ok(
            serde_json::json!({"message":"若该邮箱存在且尚未验证，验证邮件已重新发送。"}),
        )
        .into_response(),
        Err(error) => account_security_error(error),
    }
}

/// POST /api/auth/forgot-password
///
/// 发送一次性重置码。无论邮箱是否注册都返回相同响应。
#[utoipa::path(
    post,
    path = "/auth/forgot-password",
    operation_id = "account_forgot_password",
    tag = "account",
    request_body = EmailRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn forgot_password(
    State(state): State<AppState>,
    Json(body): Json<EmailRequest>,
) -> Response {
    let mail = nuono_core::service::account_mail::AccountMailAdapter::new(
        state.db.clone(),
        state.config.snapshot().await,
    );
    if !nuono_app::account_registration::AccountMail::ready(&mail) {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "邮件功能尚未就绪，请联系管理员。",
        );
    }
    let email = body.email.trim().to_ascii_lowercase();
    if let Err(message) = validate_email(&email) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    if !state
        .rate_limiter
        .check(&mail_rate_key(&email), MAIL_SENDS_PER_MINUTE)
    {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "操作过于频繁，请稍后再试");
    }
    match nuono_app::account_registration::RegistrationService::new(&state.db, &mail)
        .forgot_password(&email)
        .await
    {
        Ok(()) => ApiResponse::ok(
            serde_json::json!({"message":"若该邮箱已注册，重置码已发送，请查收并在重置密码页输入。"}),
        )
        .into_response(),
        Err(error) => account_security_error(error),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::ResetPasswordRequest)]
pub struct ResetPasswordRequest {
    pub token: String,
    #[schema(write_only, format = Password)]
    pub new_password: String,
}

/// POST /api/auth/reset-password
///
/// 核销重置令牌并设置新密码，同时撤销该账号的所有登录会话。
#[utoipa::path(
    post,
    path = "/auth/reset-password",
    operation_id = "account_reset_password",
    tag = "account",
    request_body = ResetPasswordRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn reset_password(
    State(state): State<AppState>,
    Json(body): Json<ResetPasswordRequest>,
) -> Response {
    redeem_token_response(
        &state,
        &body.token,
        TokenOperation::ResetPassword {
            password: body.new_password,
        },
        "account.password_reset",
        "重置码无效或已过期",
        "密码已重置，所有设备已被登出，请用新密码登录。",
    )
    .await
}

/// POST /api/account/email/confirm
///
/// 消费更换邮箱令牌，把新地址写入账号。确认码发往新地址，正确输入即完成验证。
#[utoipa::path(
    post,
    path = "/account/email/confirm",
    operation_id = "account_confirm_email_change",
    tag = "account",
    request_body = TokenRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn confirm_email_change(
    State(state): State<AppState>,
    Json(body): Json<TokenRequest>,
) -> Response {
    redeem_token_response(
        &state,
        &body.token,
        TokenOperation::ChangeEmail,
        "account.email_changed",
        "确认码无效或已过期",
        "邮箱已更换。",
    )
    .await
}
