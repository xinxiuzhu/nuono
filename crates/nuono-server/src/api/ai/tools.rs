//! Agent 工具调用回执与审批决策接口。

use axum::extract::State;
use serde::{Deserialize, Serialize};

use super::private_scope;
use super::runs::run_operator;
use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::RegisteredToolSummary)]
pub struct RegisteredToolSummary {
    pub name: String,
    pub description: String,
    pub risk_level: String,
    pub plugin_name: Option<String>,
}

fn tool_risk_key(risk: nuono_app::agent::tool::ToolRiskLevel) -> &'static str {
    match risk {
        nuono_app::agent::tool::ToolRiskLevel::ReadOnly => "read_only",
        nuono_app::agent::tool::ToolRiskLevel::LowRisk => "low_risk",
        nuono_app::agent::tool::ToolRiskLevel::RequiresApproval => "requires_approval",
        nuono_app::agent::tool::ToolRiskLevel::AdminOnly => "admin_only",
    }
}

fn plugin_name_from_tool_name(name: &str) -> Option<String> {
    name.strip_prefix("plugin.")
        .and_then(|rest| rest.split('.').next())
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
}

#[utoipa::path(
    get,
    path = "/tools/catalog",
    operation_id = "ai_list_tool_catalog",
    tag = "ai",
    responses((status = 200, description = "当前宿主已注册的内置与插件工具清单", body = ApiResponse<Vec<RegisteredToolSummary>>))
)]
pub(super) async fn list_tool_catalog(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<RegisteredToolSummary>> {
    let tools = state
        .manager
        .plugin_host
        .tool_registry()
        .list()
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let plugin_name = plugin_name_from_tool_name(&name);
            RegisteredToolSummary {
                name,
                description: tool.description().to_string(),
                risk_level: tool_risk_key(tool.risk_level()).to_string(),
                plugin_name,
            }
        })
        .collect();
    ApiResponse::ok(tools)
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::ai::ToolReceiptQuery)]
#[into_params(parameter_in = Query)]
pub struct ToolReceiptQuery {
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::ToolReceiptResponse)]
pub struct ToolReceiptResponse {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub session_id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: String,
    pub risk_level: String,
    pub status: String,
    pub input_json: Option<String>,
    pub output_json: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: i32,
    pub created_at: String,
}

#[utoipa::path(
    get,
    path = "/tool-receipts",
    operation_id = "ai_list_tool_receipts",
    tag = "ai",
    params(ToolReceiptQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<ToolReceiptResponse>>))
)]
pub(super) async fn list_tool_receipts(
    State(state): State<AppState>,
    Query(query): Query<ToolReceiptQuery>,
) -> crate::api::ApiReply<Vec<ToolReceiptResponse>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let rows = match state.db.list_agent_tool_receipts(limit).await {
        Ok(rows) => rows,
        Err(e) => return ApiResponse::from_err(&e),
    };

    ApiResponse::ok(
        rows.into_iter()
            .map(|mut row| {
                if private_scope(&row.session_id)
                    || row.platform == nuono_app::agent::ACCOUNT_CHAT_PLATFORM
                {
                    row.input_json = None;
                    row.output_json = None;
                    row.error = None;
                }
                ToolReceiptResponse {
                    id: row.id,
                    session_id: row.session_id,
                    bot_id: row.bot_id,
                    platform: row.platform,
                    message_type: row.message_type,
                    user_id: row.user_id,
                    group_id: row.group_id,
                    channel_id: row.channel_id,
                    message_id: row.message_id,
                    tool_name: row.tool_name,
                    risk_level: row.risk_level,
                    status: row.status,
                    input_json: row.input_json,
                    output_json: row.output_json,
                    error: row.error,
                    elapsed_ms: row.elapsed_ms,
                    created_at: nuono_core::clock::format_wire_ts(row.created_at),
                }
            })
            .collect(),
    )
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::ai::ToolApprovalQuery)]
#[into_params(parameter_in = Query)]
pub struct ToolApprovalQuery {
    pub status: Option<String>,
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::ToolApprovalResponse)]
pub struct ToolApprovalResponse {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub session_id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: String,
    pub risk_level: String,
    pub status: String,
    pub input_json: Option<String>,
    pub reason: Option<String>,
    pub decision_by: Option<String>,
    pub decision_reason: Option<String>,
    pub decided_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::ai::ToolApprovalDecisionPayload)]
pub struct ToolApprovalDecisionPayload {
    pub approved: bool,
    #[serde(rename = "decision_by")]
    pub _decision_by: Option<String>,
    pub reason: Option<String>,
}

#[utoipa::path(
    get,
    path = "/tool-approvals",
    operation_id = "ai_list_tool_approvals",
    tag = "ai",
    params(ToolApprovalQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<ToolApprovalResponse>>))
)]
pub(super) async fn list_tool_approvals(
    State(state): State<AppState>,
    Query(query): Query<ToolApprovalQuery>,
) -> crate::api::ApiReply<Vec<ToolApprovalResponse>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let status = query.status.as_deref().filter(|s| !s.trim().is_empty());
    let rows = match state.db.list_agent_tool_approvals(status, limit).await {
        Ok(rows) => rows,
        Err(e) => return ApiResponse::from_err(&e),
    };

    ApiResponse::ok(
        rows.into_iter()
            .map(|row| tool_approval_response(row.into()))
            .collect(),
    )
}

#[utoipa::path(
    get,
    path = "/tool-approvals/{id}",
    operation_id = "ai_get_tool_approval",
    tag = "ai",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ToolApprovalResponse>))
)]
pub(super) async fn get_tool_approval(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<ToolApprovalResponse> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match state.db.get_agent_tool_approval(id).await {
        Ok(Some(row)) => ApiResponse::ok(tool_approval_response(row.into())),
        Ok(None) => ApiResponse::not_found("审批不存在"),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/tool-approvals/{id}/decision",
    operation_id = "ai_decide_tool_approval",
    tag = "ai",
    request_body = ToolApprovalDecisionPayload,
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ToolApprovalResponse>))
)]
pub(super) async fn decide_tool_approval(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<i64>,
    Json(body): Json<ToolApprovalDecisionPayload>,
) -> crate::api::ApiReply<ToolApprovalResponse> {
    let caller = match caller {
        Some(caller) => caller.0,
        None if !state.config.snapshot().await.security.admin_auth_enabled => {
            nuono_core::auth::CallerIdentity::new(0, "local_system", nuono_core::auth::Role::Owner)
        }
        None => return ApiResponse::forbidden("未授权: 缺少调用者身份"),
    };
    let service = state
        .manager
        .plugin_host
        .tool_execution_service(state.config.clone());
    match service
        .decide(&caller, id, body.approved, body.reason)
        .await
    {
        Ok(row) => {
            if let Err(error) = resume_agent_reply(&state, id).await {
                tracing::warn!(%error, approval_id=id, "Approval decided; Agent continuation failed");
                return ApiResponse::from_err(&error)
                    .context("审批决定已保存，但 Agent 续跑失败，请检查运行记录，勿重复审批");
            }
            ApiResponse::ok(tool_approval_response(row))
        }
        Err(error) => ApiResponse::from_err(&error),
    }
}

pub(super) async fn resume_agent_reply(
    state: &AppState,
    id: i64,
) -> nuono_core::error::Result<Option<nuono_app::agent::run::AgentRunReply>> {
    let reply = state
        .manager
        .event_kernel
        .agent_runtime
        .resume_approval(id)
        .await?;
    if let Some(reply) = &reply
        && let Err(error) = state.manager.event_kernel.send_agent_run_reply(reply).await
    {
        tracing::warn!(%error, run_id=%reply.run_id, "Agent continued; reply delivery failed, result retained in run");
    }
    Ok(reply)
}

pub(super) fn tool_approval_response(
    row: nuono_app::agent::store::ToolApproval,
) -> ToolApprovalResponse {
    let private =
        private_scope(&row.session_id) || row.platform == nuono_app::agent::ACCOUNT_CHAT_PLATFORM;
    ToolApprovalResponse {
        id: row.id,
        session_id: row.session_id,
        bot_id: row.bot_id,
        platform: row.platform,
        message_type: row.message_type,
        user_id: row.user_id,
        group_id: row.group_id,
        channel_id: row.channel_id,
        message_id: row.message_id,
        tool_name: row.tool_name,
        risk_level: row.risk_level,
        status: row.status,
        input_json: if private { None } else { row.input_json },
        reason: if private { None } else { row.reason },
        decision_by: row.decision_by,
        decision_reason: if private { None } else { row.decision_reason },
        decided_at: row.decided_at.map(nuono_core::clock::format_wire_ts),
        created_at: nuono_core::clock::format_wire_ts(row.created_at),
    }
}
