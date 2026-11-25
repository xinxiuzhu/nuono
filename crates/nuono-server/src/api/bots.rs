use crate::api::input::{ApiJson as Json, ApiPath as Path};
use axum::extract::State;
use nuono_core::service::{
    BotStatus,
    bots::{delete_bot, get_bot_list_with_runtime},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(ping_bot))
        .routes(routes!(send_test_message))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::bots::BotPingResponse)]
pub struct BotPingResponse {
    /// 当前连接上的账号已确认在线。
    pub online: bool,
    #[serde(flatten)]
    pub bot: BotStatus,
}

impl From<BotStatus> for BotPingResponse {
    fn from(bot: BotStatus) -> Self {
        Self {
            online: bot.status == "online" && bot.transport_connected == Some(true),
            bot,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::bots::SendTestMessageBody)]
pub struct SendTestMessageBody {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub user_id: i64,
    pub message: String,
}

fn bot_sender(
    state: &AppState,
    bot_id: &str,
) -> Result<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>, String> {
    let bot_id = bot_id
        .parse::<i64>()
        .map_err(|_| "Bot ID 必须是数字".to_string())?;
    state
        .manager
        .bot_txs
        .get(&bot_id)
        .filter(|tx| !tx.is_closed())
        .map(|tx| tx.clone())
        .ok_or_else(|| format!("Bot {bot_id} 没有活动连接"))
}

#[utoipa::path(
    get,
    path = "/api/bots",
    operation_id = "bots_get_bots",
    tag = "bots",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<BotStatus>>))
)]
pub async fn get_bots(State(state): State<AppState>) -> crate::api::ApiReply<Vec<BotStatus>> {
    match get_bot_list_with_runtime(&state.db, &state.manager.bot_states).await {
        Ok(bots) => ApiResponse::ok(bots),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/bots/{bot_id}",
    operation_id = "bots_delete_bot_handler",
    tag = "bots",
    params(("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
pub async fn delete_bot_handler(
    State(state): State<AppState>,
    Path(bot_id): Path<String>,
) -> crate::api::ApiReply<()> {
    // 仅清理数据库中的历史 Bot 记录，不影响当前活跃连接或已配置的适配器实例。
    // 只要底层传输连接仍在线（即便账号处于离线或降级状态），也拒绝删除。
    if bot_id
        .parse::<i64>()
        .ok()
        .and_then(|id| state.manager.bot_states.snapshot(id))
        .is_some_and(|bot| bot.transport_connected == Some(true))
    {
        return ApiResponse::conflict("仍有活动连接，不能清除记录；请先在 OneBot 客户端断开连接");
    }
    match delete_bot(&state.db, &bot_id).await {
        Ok(()) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    post,
    path = "/{bot_id}/ping",
    operation_id = "bots_ping_bot",
    tag = "bots",
    params(("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotPingResponse>))
)]
pub async fn ping_bot(
    State(state): State<AppState>,
    caller: Option<axum::Extension<crate::middleware::CallerIdentity>>,
    Path(bot_id): Path<String>,
) -> crate::api::ApiReply<BotPingResponse> {
    let actor = caller.map(|c| c.0.username);
    let parsed_id = match bot_id.parse::<i64>() {
        Ok(id) if id > 0 => id,
        _ => return ApiResponse::bad_request("Bot ID 必须是正整数"),
    };
    match state.manager.probe_bot_status(parsed_id).await {
        Ok(bot) => {
            let response = BotPingResponse::from(bot);
            let bot = &response.bot;
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.bot_ping",
                Some(&bot_id),
                actor.as_deref(),
                json!({
                    "online": response.online,
                    "status": bot.status,
                    "reason": bot.status_reason,
                    "transport_connected": bot.transport_connected,
                }),
            )
            .await;
            // 探测动作本身执行完成时，即便探测结果为离线或状态未知，也不属于 HTTP/API 调用失败，
            // 应正常返回 200 并携带与 Bot 列表一致的状态诊断信息。
            ApiResponse::ok(response)
        }
        Err(e) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.bot_ping_failed",
                Some(&bot_id),
                actor.as_deref(),
                json!({"error": e.to_string()}),
            )
            .await;
            ApiResponse::upstream(format!("Bot 探测失败: {e}"))
        }
    }
}

#[utoipa::path(
    post,
    path = "/{bot_id}/test-message",
    operation_id = "bots_send_test_message",
    tag = "bots",
    request_body = SendTestMessageBody,
    params(("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
pub async fn send_test_message(
    State(state): State<AppState>,
    caller: Option<axum::Extension<crate::middleware::CallerIdentity>>,
    Path(bot_id): Path<String>,
    Json(body): Json<SendTestMessageBody>,
) -> crate::api::ApiReply<()> {
    let actor = caller.map(|c| c.0.username);
    if body.user_id <= 0 {
        return ApiResponse::bad_request("user_id 必须大于 0");
    }
    let message = body.message.trim();
    if message.is_empty() {
        return ApiResponse::bad_request("测试消息不能为空");
    }
    if message.chars().count() > 500 {
        return ApiResponse::bad_request("测试消息不能超过 500 个字符");
    }

    let parsed_bot_id = match bot_id.parse::<i64>() {
        Ok(id) if id > 0 => id,
        _ => return ApiResponse::bad_request("Bot ID 必须是正整数"),
    };
    let can_test = state
        .manager
        .bot_states
        .snapshot(parsed_bot_id)
        .is_some_and(|bot| can_send_test_message(&bot));
    if !can_test {
        return ApiResponse::conflict("账号未确认在线，未发送测试消息；请先检测当前状态");
    }
    let tx = match bot_sender(&state, &bot_id) {
        Ok(tx) => tx,
        Err(e) => return ApiResponse::conflict(e),
    };

    match state
        .manager
        .api_client
        .call(
            "send_private_msg",
            json!({
                "user_id": body.user_id,
                "message": message,
            }),
            &tx,
        )
        .await
    {
        Ok(data) if confirmed_message_receipt(&data) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.bot_test_message_sent",
                Some(&bot_id),
                actor.as_deref(),
                json!({"user_id": body.user_id, "message_len": message.chars().count()}),
            )
            .await;
            ApiResponse::ok(())
        }
        Ok(_) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.bot_test_message_failed",
                Some(&bot_id),
                actor.as_deref(),
                json!({"user_id": body.user_id, "error": "invalid_message_receipt"}),
            )
            .await;
            ApiResponse::upstream("客户端没有返回有效消息编号")
                .context("发送结果待确认；请先核实接收方或客户端，避免重复发送")
        }
        Err(e) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.bot_test_message_failed",
                Some(&bot_id),
                actor.as_deref(),
                json!({"user_id": body.user_id, "error": e.to_string()}),
            )
            .await;
            ApiResponse::upstream(e)
        }
    }
}

fn can_send_test_message(bot: &BotStatus) -> bool {
    bot.transport_connected == Some(true)
        && (bot.status == "online"
            || (bot.status == "degraded"
                && bot.status_reason.as_deref() == Some("platform_unhealthy")))
}

fn confirmed_message_receipt(data: &serde_json::Value) -> bool {
    data.get("message_id").is_some_and(|id| {
        id.as_i64().is_some()
            || id.as_u64().is_some()
            || id
                .as_str()
                .is_some_and(|value| value.parse::<i64>().is_ok())
    })
}

#[cfg(test)]
#[path = "../../tests/unit/api/bots.rs"]
mod tests;
