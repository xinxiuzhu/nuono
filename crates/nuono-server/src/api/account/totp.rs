use crate::AppState;
use crate::api::input::ApiJson as Json;
use crate::api::{ApiReply, ApiResponse};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use nuono_app::account_security::{AccountSecurityService, MfaOperation};
use nuono_core::auth::CallerIdentity;
use nuono_core::entity::users;
use serde::Deserialize;

use super::account_security_error;

/// 取当前会话对应的完整账号（middleware 已按 ManageOwnAccount 放行本组路由）。
pub(super) async fn own_account(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<users::Model, ApiReply<()>> {
    let Some(token) = crate::api::auth::extract_token_from_headers(headers) else {
        return Err(ApiReply::error(StatusCode::UNAUTHORIZED, "请先登录"));
    };
    match crate::api::auth::authenticated_user_checked(&state.db, &token)
        .await
        .map_err(|error| ApiResponse::from_err(&error))?
    {
        Some(user) => Ok(user),
        None => Err(ApiReply::error(StatusCode::UNAUTHORIZED, "请先登录")),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::TotpPasswordRequest)]
pub struct TotpPasswordRequest {
    #[schema(write_only, format = Password)]
    pub password: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::TotpConfirmRequest)]
pub struct TotpConfirmRequest {
    pub code: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::account::TotpRecoveryRequest)]
pub struct TotpRecoveryRequest {
    #[schema(write_only, format = Password)]
    pub password: String,
    pub code: String,
}

/// POST /api/account/totp/enroll
///
/// 开启向导第一步：生成 secret 落库（`totp_enabled` 仍为 false），明文只在这里返回
/// 一次。前端据此展示 otpauth 链接/二维码；不落日志。
#[utoipa::path(
    post,
    path = "/account/totp/enroll",
    operation_id = "account_totp_enroll",
    tag = "account",
    request_body = TotpPasswordRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn totp_enroll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpPasswordRequest>,
) -> Response {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    match AccountSecurityService::new(&state.db)
        .change_mfa(
            &CallerIdentity::from_user(&user),
            MfaOperation::Enroll {
                password: body.password,
            },
        )
        .await
    {
        Ok(result) => ApiResponse::ok(
            serde_json::json!({ "secret": result.secret, "otpauth_uri": result.otpauth_uri }),
        )
        .into_response(),
        Err(error) => account_security_error(error),
    }
}

/// POST /api/account/totp/confirm
///
/// 开启向导第二步：用验证器里当前显示的码确认，通过后启用并生成恢复码。
/// 恢复码明文只返回这一次。
#[utoipa::path(
    post,
    path = "/account/totp/confirm",
    operation_id = "account_totp_confirm",
    tag = "account",
    request_body = TotpConfirmRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn totp_confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpConfirmRequest>,
) -> Response {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    match AccountSecurityService::new(&state.db)
        .change_mfa(
            &CallerIdentity::from_user(&user),
            MfaOperation::Confirm { code: body.code },
        )
        .await
    {
        Ok(result) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "account.totp_enabled",
                None,
                Some(&result.username),
                serde_json::json!({"user_id": result.user_id.to_string()}),
            )
            .await;
            ApiResponse::ok(serde_json::json!({ "recovery_codes": result.recovery_codes }))
                .into_response()
        }
        Err(error) => account_security_error(error),
    }
}

/// POST /api/account/totp/disable
///
/// 关闭两步验证并清空 secret 与恢复码。需要当前密码：会话被盗时不能让攻击者
/// 顺手关掉唯一的多因素防线。
#[utoipa::path(
    post,
    path = "/account/totp/disable",
    operation_id = "account_totp_disable",
    tag = "account",
    request_body = TotpPasswordRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn totp_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpPasswordRequest>,
) -> Response {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    match AccountSecurityService::new(&state.db)
        .change_mfa(
            &CallerIdentity::from_user(&user),
            MfaOperation::Disable {
                password: body.password,
            },
        )
        .await
    {
        Ok(result) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "account.totp_disabled",
                None,
                Some(&result.username),
                serde_json::json!({"user_id": result.user_id.to_string()}),
            )
            .await;
            ApiResponse::ok(serde_json::json!({ "disabled": true })).into_response()
        }
        Err(error) => account_security_error(error),
    }
}

/// POST /api/account/totp/recovery
///
/// 重新生成恢复码（旧码全部作废）。要求当前密码 + 一个有效验证码：
/// 这是给「丢了恢复码但验证器还在」的人用的，验证器也没了请找管理员重置。
#[utoipa::path(
    post,
    path = "/account/totp/recovery",
    operation_id = "account_totp_regenerate_recovery",
    tag = "account",
    request_body = TotpRecoveryRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::AnyJson>))
)]
pub(super) async fn totp_regenerate_recovery(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpRecoveryRequest>,
) -> Response {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    match AccountSecurityService::new(&state.db)
        .change_mfa(
            &CallerIdentity::from_user(&user),
            MfaOperation::RegenerateRecovery {
                password: body.password,
                code: body.code,
            },
        )
        .await
    {
        Ok(result) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "account.totp_recovery_regenerated",
                None,
                Some(&result.username),
                serde_json::json!({"user_id": result.user_id.to_string()}),
            )
            .await;
            ApiResponse::ok(serde_json::json!({ "recovery_codes": result.recovery_codes }))
                .into_response()
        }
        Err(error) => account_security_error(error),
    }
}
