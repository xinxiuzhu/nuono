use axum::{
    extract::State,
    http::{StatusCode, header::CACHE_CONTROL},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use nuono_app::assistant::{
    AssistantAdmission, AssistantGrant, AssistantSettings, MAX_DAILY_LIMIT,
};
use nuono_core::{auth::Capability, db::RuntimeConfigPatch, service::assistant::AssistantView};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    api::{
        ApiReply, ApiResponse,
        input::{ApiCaller, ApiJson, ApiQuery},
    },
};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(super::companion::router())
        .routes(routes!(clear_conversation))
        .routes(routes!(clear_all))
        .routes(routes!(view))
        .routes(routes!(send))
        .routes(routes!(link, unlink))
        .routes(routes!(settings, update_settings))
        .routes(routes!(get_grant, set_grant))
}

/// 个人 AI 伴侣助手状态与近期对话（带角色卡、每日额度与账号授权控制）。
#[utoipa::path(
    get, path = "/assistant", operation_id = "assistant_view", tag = "ai",
    responses((status = 200, description = "当前账号的助手状态和近期对话", body = ApiResponse<AssistantView>))
)]
async fn view(State(state): State<AppState>, ApiCaller(caller): ApiCaller) -> impl IntoResponse {
    (
        [(CACHE_CONTROL, "no-store")],
        ApiResponse::from_result(
            state
                .manager
                .event_kernel
                .assistant
                .view(caller.user_id)
                .await,
        ),
    )
}

#[derive(Deserialize, utoipa::ToSchema)]
struct SendMessage {
    #[schema(value_type = String)]
    request_id: uuid::Uuid,
    text: Option<String>,
    retry_of: Option<String>,
}

#[utoipa::path(
    post, path = "/assistant/messages", operation_id = "assistant_send", tag = "ai",
    request_body = SendMessage,
    responses(
        (status = 200, description = "已接收，回复可从助手状态查询"),
        (status = 403, description = "助手未启用或账号没有访问授权"),
        (status = 429, description = "今日额度已用完")
    )
)]
async fn send(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiJson(body): ApiJson<SendMessage>,
) -> ApiReply<()> {
    let assistant = &state.manager.event_kernel.assistant;
    let result = match (body.text, body.retry_of) {
        (Some(text), None) => {
            assistant
                .send_web(caller.user_id, body.request_id, &text)
                .await
        }
        (None, Some(turn)) => {
            assistant
                .retry_web(caller.user_id, body.request_id, turn)
                .await
        }
        _ => return ApiReply::error(StatusCode::BAD_REQUEST, "请发送消息或选择一条失败消息重试"),
    };
    match result {
        Ok(AssistantAdmission::Accepted | AssistantAdmission::Duplicate) => ApiResponse::ok(()),
        Ok(AssistantAdmission::Unavailable) => ApiResponse::forbidden("个人助手当前不可用"),
        Ok(AssistantAdmission::Exhausted) => {
            ApiReply::error(StatusCode::TOO_MANY_REQUESTS, "今天的个人对话额度已用完")
        }
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct LinkRequest {
    qq: String,
}

#[derive(Serialize, utoipa::ToSchema)]
struct LinkCommand {
    bot: String,
    command: String,
    request_id: String,
}

#[utoipa::path(
    post, path = "/assistant/link", operation_id = "assistant_link_qq", tag = "ai",
    request_body = LinkRequest,
    responses((status = 200, description = "一次性绑定指令，有效期五分钟", body = ApiResponse<LinkCommand>))
)]
async fn link(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    headers: axum::http::HeaderMap,
    ApiJson(body): ApiJson<LinkRequest>,
) -> ApiReply<LinkCommand> {
    ApiResponse::from_result(
        state
            .manager
            .event_kernel
            .assistant
            .begin_qq_link(
                caller.user_id,
                &body.qq,
                &super::auth::extract_token_from_headers(&headers).unwrap_or_default(),
            )
            .await
            .map(|(bot, issued)| LinkCommand {
                bot,
                command: issued.command,
                request_id: issued.request.id,
            }),
    )
}

#[utoipa::path(
    delete, path = "/assistant/link", operation_id = "assistant_unlink_qq", tag = "ai",
    responses((status = 200, description = "已解除绑定"))
)]
async fn unlink(State(state): State<AppState>, ApiCaller(caller): ApiCaller) -> ApiReply<()> {
    ApiResponse::from_result(
        state
            .manager
            .event_kernel
            .assistant
            .unlink_qq(caller.user_id)
            .await,
    )
}

#[derive(Serialize, utoipa::ToSchema)]
struct SettingsView {
    #[serde(flatten)]
    settings: AssistantSettings,
    grant_required_by_policy: bool,
}

impl From<AssistantSettings> for SettingsView {
    fn from(settings: AssistantSettings) -> Self {
        let grant_required_by_policy = nuono_app::entitlement::requires_assistant_grant();
        Self {
            settings,
            grant_required_by_policy,
        }
    }
}

#[utoipa::path(
    get, path = "/assistant/settings", operation_id = "assistant_settings", tag = "ai",
    responses((status = 200, description = "个人助手设置", body = ApiResponse<SettingsView>))
)]
async fn settings(State(state): State<AppState>) -> ApiReply<SettingsView> {
    ApiResponse::ok(state.config.snapshot().await.ai.assistant.clone().into())
}

#[utoipa::path(
    put, path = "/assistant/settings", operation_id = "assistant_update_settings", tag = "ai",
    request_body = AssistantSettings,
    responses((status = 200, description = "已保存", body = ApiResponse<SettingsView>))
)]
async fn update_settings(
    State(state): State<AppState>,
    ApiJson(settings): ApiJson<AssistantSettings>,
) -> ApiReply<SettingsView> {
    if let Err(error) = settings.validate() {
        return ApiResponse::from_err(&error);
    }
    let result = state
        .config
        .update_patch(RuntimeConfigPatch {
            settings: json!({"ai": {"assistant": settings}}),
            secrets: Vec::new(),
        })
        .await;
    match result {
        Ok(()) => ApiResponse::ok(state.config.snapshot().await.ai.assistant.clone().into()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Deserialize)]
struct GrantQuery {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    account_id: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
struct GrantView {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    account_id: i64,
    grant: Option<AssistantGrant>,
}

#[utoipa::path(
    get, path = "/assistant/grants", operation_id = "assistant_get_grant", tag = "ai",
    params(("account_id" = i64, Query, description = "网站账号 ID")),
    responses((status = 200, description = "指定账号已保存的助手授权", body = ApiResponse<GrantView>))
)]
async fn get_grant(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiQuery(query): ApiQuery<GrantQuery>,
) -> impl IntoResponse {
    let reply =
        if !caller.has(Capability::ManageRuntime) {
            ApiResponse::forbidden(Capability::ManageRuntime.denial_message())
        } else if query.account_id <= 0 {
            ApiResponse::bad_request("账号 ID 无效")
        } else {
            ApiResponse::from_result(state.db.assistant_grant(query.account_id).await.map(
                |grant| GrantView {
                    account_id: query.account_id,
                    grant,
                },
            ))
        };
    ([(CACHE_CONTROL, "no-store")], reply)
}

#[derive(Deserialize, utoipa::ToSchema)]
struct SetGrant {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    account_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    daily_limit: Option<u32>,
}

#[utoipa::path(
    put, path = "/assistant/grants", operation_id = "assistant_set_grant", tag = "ai",
    request_body = SetGrant,
    responses((status = 200, description = "已更新指定账号的助手访问授权"))
)]
async fn set_grant(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiJson(body): ApiJson<SetGrant>,
) -> ApiReply<()> {
    if !caller.has(Capability::ManageRuntime) {
        return ApiResponse::forbidden(Capability::ManageRuntime.denial_message());
    }
    let grant = match (body.expires_at, body.daily_limit) {
        (None, None) => None,
        (Some(until), Some(limit))
            if until > Utc::now() && (1..=MAX_DAILY_LIMIT).contains(&limit) =>
        {
            Some((until, limit))
        }
        _ => return ApiResponse::bad_request("请填写未来的到期时间和 1 至 500 条每日额度"),
    };
    if let Err(error) = state.db.set_assistant_grant(body.account_id, grant).await {
        return ApiResponse::from_err(&error);
    }
    let _ = nuono_core::audit::record_event(
        &state.db, "admin.assistant_grant_updated", None, Some(&caller.username),
        json!({"account_id": body.account_id.to_string(), "expires_at": body.expires_at.map(nuono_core::clock::format_wire_ts), "daily_limit": body.daily_limit}),
    ).await;
    ApiResponse::ok(())
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ClearConversation {
    card_id: String,
}

#[utoipa::path(post, path = "/assistant/data/clear-conversation", operation_id = "assistant_clear_conversation", tag = "ai", request_body = ClearConversation, responses((status = 200, description = "清空所选角色对话，保留长期记忆")))]
async fn clear_conversation(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiJson(input): ApiJson<ClearConversation>,
) -> ApiReply<()> {
    ApiResponse::from_result(
        state
            .manager
            .event_kernel
            .assistant
            .clear_personal_data(caller.user_id, Some(&input.card_id))
            .await,
    )
}

#[utoipa::path(post, path = "/assistant/data/clear-all", operation_id = "assistant_clear_all", tag = "ai", responses((status = 200, description = "清除全部角色的个人聊天与记忆，保留账号和角色设定")))]
async fn clear_all(State(state): State<AppState>, ApiCaller(caller): ApiCaller) -> ApiReply<()> {
    ApiResponse::from_result(
        state
            .manager
            .event_kernel
            .assistant
            .clear_personal_data(caller.user_id, None)
            .await,
    )
}
