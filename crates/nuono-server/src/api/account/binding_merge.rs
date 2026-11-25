use crate::AppState;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use crate::api::{ApiReply, ApiResponse};
use axum::{extract::State, http::HeaderMap};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, utoipa::ToSchema)]
pub(super) struct BeginPlatformBinding {
    bot: serde_json::Value,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub(super) struct IdentityRevision {
    revision: String,
}
#[derive(Deserialize, utoipa::ToSchema)]
pub(super) struct BindingConfirmation {
    revision: String,
    data_claim_digest: Option<String>,
}

fn binding_session(headers: &HeaderMap) -> nuono_core::error::Result<String> {
    crate::api::auth::extract_token_from_headers(headers)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| nuono_core::error::NuonoError::PermissionDenied("绑定需要登录会话".into()))
}

#[utoipa::path(post, path = "/account/bindings", operation_id = "account_begin_platform_binding", tag = "auth",
    request_body = BeginPlatformBinding,
    responses((status = 200, description = "返回一次性绑定指令", body = ApiResponse<nuono_core::db::PlatformBindingIssued>)))]
pub(super) async fn begin_platform_binding(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Json(body): Json<BeginPlatformBinding>,
) -> ApiReply<nuono_core::db::PlatformBindingIssued> {
    let result = async {
        let bot: nuono_core::domain::BotIdentity = serde_json::from_value(body.bot)
            .map_err(|_| nuono_core::error::NuonoError::Validation("机器人完整身份无效".into()))?;
        state
            .db
            .begin_platform_binding(caller.user_id, bot, &binding_session(&headers)?, None)
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(get, path = "/account/bindings/{id}", operation_id = "account_platform_binding_status", tag = "auth",
    responses((status = 200, description = "原会话查看候选身份", body = ApiResponse<nuono_core::db::PlatformBindingView>)))]
pub(super) async fn platform_binding_status(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiReply<nuono_core::db::PlatformBindingView> {
    let result = async {
        state
            .db
            .platform_binding(caller.user_id, &id, &binding_session(&headers)?)
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(post, path = "/account/bindings/{id}", operation_id = "account_confirm_platform_binding", tag = "auth",
    request_body = BindingConfirmation, responses((status = 200, description = "确认绑定身份")))]
pub(super) async fn confirm_platform_binding(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<BindingConfirmation>,
) -> ApiReply<()> {
    let result = async {
        let revision = positive_decimal(&body.revision, "身份版本无效")?;
        state
            .db
            .confirm_platform_binding_with_claim(
                caller.user_id,
                &id,
                &binding_session(&headers)?,
                revision,
                body.data_claim_digest.as_deref(),
            )
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(delete, path = "/account/bindings/{id}", operation_id = "account_cancel_platform_binding", tag = "auth",
    responses((status = 200, description = "作废绑定请求")))]
pub(super) async fn cancel_platform_binding(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiReply<()> {
    let result = async {
        state
            .db
            .cancel_platform_binding(caller.user_id, &id, &binding_session(&headers)?)
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(get, path = "/account/identities", operation_id = "account_platform_identities", tag = "auth",
    responses((status = 200, description = "当前账号的平台绑定", body = ApiResponse<Vec<nuono_core::db::PlatformIdentityView>>)))]
pub(super) async fn platform_identities(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> ApiReply<Vec<nuono_core::db::PlatformIdentityView>> {
    ApiResponse::from_result(state.db.platform_identities(caller.user_id).await)
}

#[utoipa::path(delete, path = "/account/identities/{id}", operation_id = "account_unbind_platform_identity", tag = "auth",
    request_body = IdentityRevision, responses((status = 200, description = "解除一个平台绑定，保留账号配置")))]
pub(super) async fn unbind_platform_identity(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<IdentityRevision>,
) -> ApiReply<()> {
    let result = async {
        let id = positive_decimal(&id, "身份编号无效")?;
        let revision = positive_decimal(&body.revision, "身份版本无效")?;
        state
            .db
            .unbind_platform_identity_in_session(
                caller.user_id,
                id,
                revision,
                &binding_session(&headers)?,
            )
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

fn positive_decimal(value: &str, message: &str) -> nuono_core::error::Result<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0 && number.to_string() == value)
        .ok_or_else(|| nuono_core::error::NuonoError::Validation(message.into()))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MergeRequest {
    source_username: String,
    source_password: String,
    source_second_factor: Option<String>,
    target_password: String,
    target_second_factor: Option<String>,
}
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MergeConfirmation {
    revision: String,
    plan_digest: String,
}

#[utoipa::path(post,path="/account/merges",operation_id="account_merge_prepare",tag="auth",request_body=MergeRequest,
    responses((status=200,description="验证两个账号并生成只读数据预览",body=ApiResponse<nuono_core::db::AccountMergeView>)))]
pub(super) async fn prepare_account_merge(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Json(body): Json<MergeRequest>,
) -> ApiReply<nuono_core::db::AccountMergeView> {
    let result = async {
        let source = state
            .db
            .find_user_by_identifier(&body.source_username)
            .await?
            .ok_or_else(|| nuono_core::NuonoError::PermissionDenied("来源账号凭据不正确".into()))?;
        let _guard = state
            .manager
            .event_kernel
            .assistant
            .guard_account_merge(source.id, caller.user_id)?;
        state
            .db
            .prepare_account_merge(
                caller.user_id,
                &binding_session(&headers)?,
                nuono_core::db::MergeCredentials {
                    source_username: &body.source_username,
                    source_password: &body.source_password,
                    source_second_factor: body.source_second_factor.as_deref(),
                    target_password: &body.target_password,
                    target_second_factor: body.target_second_factor.as_deref(),
                },
            )
            .await
    }
    .await;
    ApiResponse::from_result(result)
}
#[utoipa::path(get,path="/account/merges/{id}",operation_id="account_merge_status",tag="auth",
    responses((status=200,description="查看当前账号的合并预览或回执",body=ApiResponse<nuono_core::db::AccountMergeView>)))]
pub(super) async fn account_merge_status(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    Path(id): Path<String>,
) -> ApiReply<nuono_core::db::AccountMergeView> {
    ApiResponse::from_result(state.db.account_merge_status(caller.user_id, &id).await)
}
#[utoipa::path(post,path="/account/merges/{id}/confirm",operation_id="account_merge_confirm",tag="auth",request_body=MergeConfirmation,
    responses((status=200,description="原子合并并返回持久回执；两个账号的旧会话全部失效",body=ApiResponse<nuono_core::db::AccountMergeView>)))]
pub(super) async fn confirm_account_merge(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MergeConfirmation>,
) -> ApiReply<nuono_core::db::AccountMergeView> {
    let result = async {
        let revision = positive_decimal(&body.revision, "合并版本无效")?;
        let operation = state.db.account_merge_status(caller.user_id, &id).await?;
        let source = positive_decimal(&operation.source_user_id, "合并账号编号无效")?;
        let _guard = state
            .manager
            .event_kernel
            .assistant
            .guard_account_merge(source, caller.user_id)?;
        state
            .db
            .confirm_account_merge(
                caller.user_id,
                &binding_session(&headers)?,
                &id,
                revision,
                &body.plan_digest,
            )
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(get,path="/account/sessions",operation_id="account_sessions",tag="auth",
    responses((status=200,description="当前账号的会话",body=ApiResponse<Vec<nuono_core::db::AccountSessionView>>)))]
pub(super) async fn account_sessions(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
) -> ApiReply<Vec<nuono_core::db::AccountSessionView>> {
    let result = async {
        state
            .db
            .account_sessions(caller.user_id, &binding_session(&headers)?)
            .await
    }
    .await;
    ApiResponse::from_result(result)
}
#[utoipa::path(delete,path="/account/sessions/{id}",operation_id="account_session_revoke",tag="auth",responses((status=200,description="撤销自己的一条会话")))]
pub(super) async fn revoke_account_session(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiReply<()> {
    let result = async {
        state
            .db
            .revoke_account_session(caller.user_id, &binding_session(&headers)?, &id)
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(delete,path="/account/merges/{id}",operation_id="account_merge_cancel",tag="auth",responses((status=200,description="取消未完成合并并作废证明")))]
pub(super) async fn cancel_account_merge(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiReply<()> {
    let result = async {
        state
            .db
            .cancel_account_merge(caller.user_id, &binding_session(&headers)?, &id)
            .await
    }
    .await;
    ApiResponse::from_result(result)
}

#[utoipa::path(get,path="/account/merges",operation_id="account_merge_history",tag="auth",responses((status=200,description="最近二十次合并预览和持久回执",body=ApiResponse<Vec<nuono_core::db::AccountMergeView>>)))]
pub(super) async fn account_merge_history(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> ApiReply<Vec<nuono_core::db::AccountMergeView>> {
    ApiResponse::from_result(state.db.account_merge_history(caller.user_id).await)
}

#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct BindingBotOption {
    bot: serde_json::Value,
    label: String,
    platform: String,
    bot_self_id: String,
    instance_id: String,
}
#[utoipa::path(get,path="/account/binding-bots",operation_id="account_binding_bots",tag="auth",responses((status=200,description="可以接收身份绑定指令的已连接机器人",body=ApiResponse<Vec<BindingBotOption>>)))]
pub(super) async fn binding_bots(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(_caller): crate::api::input::ApiCaller,
) -> ApiReply<Vec<BindingBotOption>> {
    let result = state
        .manager
        .event_kernel
        .action_router()
        .connected_routes()
        .into_iter()
        .map(|route| {
            let bot = nuono_plugin::core::runtime_transport::route_identity(&route)
                .map_err(|error| nuono_core::error::NuonoError::Validation(error.to_string()))?;
            Ok(BindingBotOption {
                bot: serde_json::to_value(bot)?,
                label: format!("{} · {}", route.platform, route.bot_self_id),
                platform: route.platform,
                bot_self_id: route.bot_self_id,
                instance_id: route.instance_id,
            })
        })
        .collect::<nuono_core::error::Result<Vec<_>>>();
    ApiResponse::from_result(result)
}
