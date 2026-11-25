use crate::api::input::ApiJson as Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(send_message))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::messages::SendMessagePayload)]
pub struct SendMessagePayload {
    #[serde(default)]
    pub bot_id: String,
    #[serde(default)]
    pub target_type: String,
    #[serde(default)]
    pub target_id: String,
    pub message: String,
    #[serde(default)]
    pub message_type: nuono_app::managed_messages::MessageType,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub route: Option<nuono_app::transport::RuntimeBotRoute>,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub target: Option<nuono_core::domain::DeliveryTarget>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::messages::SendMessageReceipt)]
pub struct SendMessageReceipt {
    pub bot_id: String,
    pub target_type: String,
    pub target_id: String,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub message_id: Option<i64>,
    pub message_ids: Vec<String>,
    pub degradations: Vec<String>,
    pub raw: Value,
}

#[utoipa::path(
    post,
    path = "/send",
    operation_id = "messages_send_message",
    tag = "messages",
    request_body = SendMessagePayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<SendMessageReceipt>))
)]
async fn send_message(
    State(state): State<AppState>,
    // 本机关闭管理认证时允许匿名调用；存在有效会话则记录真实身份。
    // 匿名请求没有审计主体，不能硬取 Extension。
    caller: Option<axum::Extension<crate::middleware::CallerIdentity>>,
    Json(body): Json<SendMessagePayload>,
) -> crate::api::ApiReply<SendMessageReceipt> {
    let router = state.manager.event_kernel.action_router();
    let request = match normalize_message(&body, &router.connected_routes()) {
        Ok(request) => request,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let target_type = match &request.target {
        nuono_core::domain::DeliveryTarget::Actor { .. } => "private",
        nuono_core::domain::DeliveryTarget::Conversation {
            conversation: nuono_core::domain::ConversationRef::Group { .. },
        } => "group",
        _ => "channel",
    }
    .to_string();
    let target_id = request
        .target
        .identities()
        .next()
        .map(|id| id.value().as_str().to_owned())
        .unwrap_or_default();
    let actor = caller.as_ref().map(|c| c.0.username.clone());
    let authority = match caller {
        Some(caller) => nuono_app::action::ActionAuthority::Dashboard(caller.0),
        None if !state.config.snapshot().await.security.admin_auth_enabled => {
            nuono_app::action::ActionAuthority::Runtime {
                stage: "dashboard_local".into(),
            }
        }
        None => return ApiResponse::forbidden("请先登录"),
    };
    match nuono_core::service::managed_messages::send(&router, &request, authority).await {
        Ok(receipt) => {
            let message_id = receipt
                .message_ids
                .first()
                .filter(|_| receipt.message_ids.len() == 1)
                .and_then(|id| id.parse::<i64>().ok());
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.message_sent",
                Some(&request.route.bot_self_id),
                actor.as_deref(),
                json!({
                    "route": request.route,
                    "target_type": target_type,
                    "target_id": target_id,
                    "message_len": request.message.chars().count(),
                    "message_id": message_id.map(|id|id.to_string()),
                }),
            )
            .await;
            ApiResponse::ok(SendMessageReceipt {
                bot_id: request.route.bot_self_id,
                target_type,
                target_id,
                message_id,
                message_ids: receipt.message_ids,
                degradations: receipt.degradations,
                raw: receipt.raw,
            })
        }
        Err(e) => {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "admin.message_send_failed",
                Some(&request.route.bot_self_id),
                actor.as_deref(),
                json!({
                    "route": request.route,
                    "target_type": target_type,
                    "target_id": target_id,
                    "message_len": request.message.chars().count(),
                    "error": e.to_string(),
                }),
            )
            .await;
            ApiResponse::upstream(format!("发送失败: {}", e))
        }
    }
}

fn normalize_message(
    body: &SendMessagePayload,
    routes: &[nuono_app::transport::RuntimeBotRoute],
) -> nuono_core::Result<nuono_app::managed_messages::ManagedMessage> {
    use nuono_core::NuonoError;
    use nuono_core::domain::{
        ConversationRef, DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, Platform,
    };
    let (route, target) = match (&body.route, &body.target) {
        (Some(route), Some(target)) => (route.clone(), target.clone()),
        (None, None) => {
            let bot_id = body
                .bot_id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| NuonoError::Validation("bot_id 必须是有效数字".into()))?
                .to_string();
            let target_id = body
                .target_id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| NuonoError::Validation("target_id 必须是有效数字".into()))?
                .to_string();
            let mut matches = routes.iter().filter(|route| {
                matches!(route.platform.as_str(), "onebot" | "OneBot v11")
                    && route.bot_self_id == bot_id
            });
            let route = matches
                .next()
                .ok_or_else(|| NuonoError::Connection("Bot 未在线".into()))?
                .clone();
            if matches.next().is_some() {
                return Err(NuonoError::Conflict(
                    "存在多个同名机器人，请明确选择实例".into(),
                ));
            }
            let kind = match body.target_type.as_str() {
                "group" => IdentityKind::Group,
                "private" => IdentityKind::User,
                _ => {
                    return Err(NuonoError::Validation(
                        "target_type 只能是 group 或 private".into(),
                    ));
                }
            };
            let identity = IdentityRef::new(
                Platform::OneBot,
                kind,
                IdentityScheme::Native,
                target_id,
                None::<String>,
            )
            .map_err(|e| NuonoError::Validation(e.to_string()))?;
            let target = if body.target_type == "group" {
                DeliveryTarget::conversation(
                    ConversationRef::group(identity)
                        .map_err(|e| NuonoError::Validation(e.to_string()))?,
                )
            } else {
                DeliveryTarget::actor(identity)
            }
            .map_err(|e| NuonoError::Validation(e.to_string()))?;
            (route, target)
        }
        _ => {
            return Err(NuonoError::Validation(
                "route 与 target 必须同时提供".into(),
            ));
        }
    };
    let request = nuono_app::managed_messages::ManagedMessage {
        route,
        target,
        message: body.message.clone(),
        message_type: body.message_type,
    };
    request.validate()?;
    Ok(request)
}

#[cfg(test)]
#[path = "../../tests/unit/api/messages/tests.rs"]
mod tests;
