//! 站点管理员与所有者视角下的全局平台身份集中管理接口。
//!
//! 聊天端权限采用单轨模型：所有平台身份归属于具体站点账号，并自动继承该账号的角色权限。
//! 列表查询对管理员与所有者开放；跨账号的手动绑定与解绑仅允许站点所有者（`AssignRoles`）执行，
//! 防止普通管理员通过将自己的聊天账号绑到所有者名下实现提权。

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use axum::{
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use nuono_core::domain::{IdentityKind, IdentityRef, IdentityScheme, Platform};
use nuono_core::error::NuonoError;
use serde::Deserialize;

use super::{require_role_assignment, require_user_management};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::account::AdminBindPlatformIdentityRequest)]
pub(super) struct AdminBindPlatformIdentityRequest {
    /// 平台标识：onebot / qq_official / telegram / discord / feishu
    pub platform: String,
    /// 平台原生用户 ID
    pub native_user_id: String,
    /// 可选命名空间：官 Q 私聊为 `app_id`，群聊为 `app_id/groups/group_openid`；飞书为 `app_id`
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::account::AdminUnbindIdentityRequest)]
pub(super) struct AdminUnbindIdentityRequest {
    pub revision: String,
}

fn parse_admin_identity(
    body: &AdminBindPlatformIdentityRequest,
) -> nuono_core::error::Result<IdentityRef> {
    let platform_key = body.platform.trim().to_ascii_lowercase();
    let native_user_id = body.native_user_id.trim();
    if native_user_id.is_empty()
        || native_user_id.len() > 128
        || native_user_id.chars().any(char::is_whitespace)
    {
        return Err(NuonoError::Validation(
            "平台用户 ID 不能为空、含空白或超过 128 个字符".into(),
        ));
    }
    let namespace = body
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(ns) = namespace
        && (ns.len() > 256 || ns.chars().any(char::is_whitespace))
    {
        return Err(NuonoError::Validation(
            "命名空间不能含空白或超过 256 个字符".into(),
        ));
    }

    let (platform, scheme, required_namespace) = match platform_key.as_str() {
        "onebot" => (Platform::OneBot, IdentityScheme::Native, false),
        "telegram" => (Platform::Telegram, IdentityScheme::Numeric, false),
        "discord" => (Platform::Discord, IdentityScheme::Snowflake, false),
        "qq_official" => (Platform::QqOfficial, IdentityScheme::OpenId, true),
        "feishu" => (Platform::Feishu, IdentityScheme::OpenId, true),
        _ => {
            return Err(NuonoError::Validation(
                "不支持的聊天平台，请选择 OneBot、QQ 官方、Telegram、Discord 或飞书".into(),
            ));
        }
    };

    if required_namespace && namespace.is_none() {
        return Err(NuonoError::Validation(
            "该平台需要填写应用或群聊命名空间，或改用聊天端指令自动绑定".into(),
        ));
    }
    if !required_namespace && namespace.is_some() {
        return Err(NuonoError::Validation(
            "该平台的身份全站唯一，不需要填写命名空间".into(),
        ));
    }

    IdentityRef::new(
        platform,
        IdentityKind::User,
        scheme,
        native_user_id,
        namespace.map(str::to_owned),
    )
    .map_err(|_| match platform_key.as_str() {
        "telegram" => NuonoError::Validation("Telegram 用户 ID 必须是 64 位整数".into()),
        "discord" => NuonoError::Validation("Discord 用户 ID 必须是无符号 64 位雪花编号".into()),
        _ => NuonoError::Validation("平台身份格式无效，请检查后重试".into()),
    })
}

fn positive_decimal(value: &str, message: &str) -> nuono_core::error::Result<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0 && number.to_string() == value)
        .ok_or_else(|| NuonoError::Validation(message.into()))
}

/// GET /api/users/identities
///
/// 列出全站已绑定并验证的平台身份与生效聊天权限（管理员及以上可读）。
#[utoipa::path(
    get,
    path = "/users/identities",
    operation_id = "account_list_user_identities",
    tag = "account",
    responses((status = 200, description = "全站已绑定平台身份列表", body = ApiResponse<Vec<nuono_core::db::AccountPlatformIdentityView>>))
)]
pub(super) async fn list_user_identities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = require_user_management(&state, &headers).await {
        return denied.into_response();
    }
    ApiResponse::from_result(state.db.list_account_platform_identities().await).into_response()
}

/// POST /api/users/{id}/identities
///
/// 由站点所有者直接将平台身份绑定到指定活跃账号。
#[utoipa::path(
    post,
    path = "/users/{id}/identities",
    operation_id = "account_admin_bind_user_identity",
    tag = "account",
    params(("id" = i64, Path)),
    request_body = AdminBindPlatformIdentityRequest,
    responses((status = 200, description = "绑定成功的平台身份", body = ApiResponse<nuono_core::db::PlatformIdentityView>))
)]
pub(super) async fn admin_bind_user_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(target_user_id): Path<i64>,
    Json(body): Json<AdminBindPlatformIdentityRequest>,
) -> Response {
    let actor = match require_role_assignment(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    let result = async {
        let identity = parse_admin_identity(&body)?;
        state
            .db
            .admin_bind_platform_identity(actor.id, target_user_id, &identity)
            .await
    }
    .await;
    ApiResponse::from_result(result).into_response()
}

/// DELETE /api/users/{id}/identities/{identity_id}
///
/// 由站点所有者解除指定账号的某个外部平台身份绑定。
#[utoipa::path(
    delete,
    path = "/users/{id}/identities/{identity_id}",
    operation_id = "account_admin_unbind_user_identity",
    tag = "account",
    params(("id" = i64, Path), ("identity_id" = String, Path)),
    request_body = AdminUnbindIdentityRequest,
    responses((status = 200, description = "解绑成功"))
)]
pub(super) async fn admin_unbind_user_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((target_user_id, identity_id)): Path<(i64, String)>,
    Json(body): Json<AdminUnbindIdentityRequest>,
) -> Response {
    let actor = match require_role_assignment(&state, &headers).await {
        Ok(user) => user,
        Err(denied) => return denied.into_response(),
    };
    let result = async {
        let identity_id = positive_decimal(&identity_id, "身份编号无效")?;
        let revision = positive_decimal(&body.revision, "身份版本无效")?;
        state
            .db
            .admin_unbind_platform_identity(actor.id, target_user_id, identity_id, revision)
            .await
    }
    .await;
    ApiResponse::from_result(result).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_admin_identity_validates_platform_scheme_and_namespace_rules() {
        let onebot = parse_admin_identity(&AdminBindPlatformIdentityRequest {
            platform: "onebot".into(),
            native_user_id: " 2609145334 ".into(),
            namespace: None,
        })
        .unwrap();
        assert_eq!(*onebot.platform(), Platform::OneBot);
        assert_eq!(*onebot.scheme(), IdentityScheme::Native);
        assert_eq!(onebot.value().as_str(), "2609145334");
        assert!(onebot.namespace().is_none());

        assert!(
            parse_admin_identity(&AdminBindPlatformIdentityRequest {
                platform: "telegram".into(),
                native_user_id: "not-a-number".into(),
                namespace: None,
            })
            .is_err()
        );
        assert!(
            parse_admin_identity(&AdminBindPlatformIdentityRequest {
                platform: "qq_official".into(),
                native_user_id: "USER_OPENID".into(),
                namespace: None,
            })
            .is_err()
        );
        let qq = parse_admin_identity(&AdminBindPlatformIdentityRequest {
            platform: "qq_official".into(),
            native_user_id: "MEMBER_OPENID".into(),
            namespace: Some("1020001/groups/GROUP_1".into()),
        })
        .unwrap();
        assert_eq!(*qq.platform(), Platform::QqOfficial);
        assert_eq!(*qq.scheme(), IdentityScheme::OpenId);
        assert_eq!(
            qq.namespace().map(|v| v.as_str()),
            Some("1020001/groups/GROUP_1")
        );
    }
}
