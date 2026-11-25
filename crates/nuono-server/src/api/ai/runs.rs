//! Agent 运行任务查询、引导、取消、汇总与审批恢复接口。

use axum::extract::State;
use serde::{Deserialize, Serialize};

use super::private_scope;
use super::tools::resume_agent_reply;
use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::AgentRunResponse)]
pub struct AgentRunResponse {
    pub id: String,
    pub title: String,
    pub task: String,
    pub platform: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_id: i64,
    pub parent_id: Option<String>,
    pub node_id: Option<String>,
    pub depends_on: Vec<String>,
    pub session_id: String,
    pub status: String,
    pub rounds: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub approval_id: Option<i64>,
    pub reply: Option<String>,
    pub budget: AgentRunBudgetResponse,
    pub children: Vec<AgentChildResponse>,
}
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::AgentRunBudgetResponse)]
pub struct AgentRunBudgetResponse {
    pub max_rounds: u32,
    pub max_tool_calls: u32,
    pub max_tokens: u64,
    pub timeout_ms: u64,
}
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::AgentChildResponse)]
pub struct AgentChildResponse {
    pub id: String,
    pub node_id: Option<String>,
    pub depends_on: Vec<String>,
    pub title: String,
    pub status: String,
    pub rounds: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub approval_id: Option<i64>,
    pub reply: Option<String>,
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AgentRunListQuery {
    pub status: Option<String>,
    /// 返回最近根任务的数量上限（1..=100，默认 50）。
    pub limit: Option<u64>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::AgentRunSummaryResponse)]
pub struct AgentRunSummaryResponse {
    pub id: String,
    pub title: String,
    pub status: String,
    pub platform: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_id: i64,
    pub rounds: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub approval_id: Option<i64>,
    pub child_count: u64,
    pub updated_at: String,
    pub reply_preview: Option<String>,
}

impl From<nuono_app::agent::run::AgentRunSummary> for AgentRunSummaryResponse {
    fn from(mut run: nuono_app::agent::run::AgentRunSummary) -> Self {
        if run.platform == nuono_app::agent::ACCOUNT_CHAT_PLATFORM {
            run.title = "个人助手任务".into();
            run.reply_preview = None;
        }
        Self {
            id: run.id,
            title: run.title,
            status: run.status,
            platform: run.platform,
            bot_id: run.bot_id,
            rounds: run.rounds,
            tool_calls: run.tool_calls,
            tokens: run.tokens,
            approval_id: run.approval_id,
            child_count: run.child_count,
            updated_at: run.updated_at,
            reply_preview: run.reply_preview,
        }
    }
}

impl From<nuono_app::agent::run::AgentRunView> for AgentRunResponse {
    fn from(mut run: nuono_app::agent::run::AgentRunView) -> Self {
        if run.platform == nuono_app::agent::ACCOUNT_CHAT_PLATFORM || private_scope(&run.session_id)
        {
            run.title = "个人助手任务".into();
            run.task.clear();
            run.reply = None;
            run.node_id = None;
            run.depends_on.clear();
            for child in &mut run.children {
                child.title = "个人助手子任务".into();
                child.reply = None;
                child.node_id = None;
                child.depends_on.clear();
            }
        }
        Self {
            id: run.id,
            title: run.title,
            task: run.task,
            platform: run.platform,
            bot_id: run.bot_id,
            parent_id: run.parent_id,
            node_id: run.node_id,
            depends_on: run.depends_on,
            session_id: run.session_id,
            status: run.status,
            rounds: run.rounds,
            tool_calls: run.tool_calls,
            tokens: run.tokens,
            approval_id: run.approval_id,
            reply: run.reply,
            budget: AgentRunBudgetResponse {
                max_rounds: run.limits.max_rounds,
                max_tool_calls: run.limits.max_tool_calls,
                max_tokens: run.limits.max_tokens,
                timeout_ms: run.limits.timeout.as_millis().min(u64::MAX as u128) as u64,
            },
            children: run
                .children
                .into_iter()
                .map(|child| AgentChildResponse {
                    id: child.id,
                    node_id: child.node_id,
                    depends_on: child.depends_on,
                    title: child.title,
                    status: child.status,
                    rounds: child.rounds,
                    tool_calls: child.tool_calls,
                    tokens: child.tokens,
                    approval_id: child.approval_id,
                    reply: child.reply,
                })
                .collect(),
        }
    }
}
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::ai::AgentSteerPayload)]
pub struct AgentSteerPayload {
    pub text: String,
}

pub(super) async fn run_operator(
    state: &AppState,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
) -> Result<(), String> {
    authorize_run_operator(
        state.config.snapshot().await.security.admin_auth_enabled,
        caller.as_ref().map(|caller| &caller.0),
    )
}

pub(super) fn authorize_run_operator(
    admin_auth_enabled: bool,
    caller: Option<&nuono_core::auth::CallerIdentity>,
) -> Result<(), String> {
    match caller {
        Some(caller) if caller.has(nuono_core::auth::Capability::ManageRuntime) => Ok(()),
        None if !admin_auth_enabled => Ok(()),
        _ => Err("未授权：需要运行管理权限".into()),
    }
}

#[utoipa::path(get, path="/runs", operation_id="ai_list_agent_runs", tag="ai", params(AgentRunListQuery),
    responses((status=200, body=ApiResponse<Vec<AgentRunSummaryResponse>>)))]
pub(super) async fn list_agent_runs(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Query(query): Query<AgentRunListQuery>,
) -> crate::api::ApiReply<Vec<AgentRunSummaryResponse>> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match state
        .manager
        .event_kernel
        .agent_runtime
        .list_runs(query.status.as_deref(), query.limit.unwrap_or(50))
        .await
    {
        Ok(runs) => ApiResponse::ok(runs.into_iter().map(Into::into).collect()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(get, path="/runs/{id}", operation_id="ai_get_agent_run", tag="ai", params(("id"=String, Path)),
    responses((status=200, body=ApiResponse<AgentRunResponse>)))]
pub(super) async fn get_agent_run(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<AgentRunResponse> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match state.manager.event_kernel.agent_runtime.get_run(&id).await {
        Ok(Some(run)) => ApiResponse::ok(run.into()),
        Ok(None) => ApiResponse::not_found("任务不存在"),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(post, path="/runs/{id}/steer", operation_id="ai_steer_agent_run", tag="ai", params(("id"=String, Path)), request_body=AgentSteerPayload,
    responses((status=200, body=ApiResponse<serde_json::Value>)))]
pub(super) async fn steer_agent_run(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<String>,
    Json(body): Json<AgentSteerPayload>,
) -> crate::api::ApiReply<serde_json::Value> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match state.manager.event_kernel.agent_runtime.get_run(&id).await {
        Ok(Some(run))
            if private_scope(&run.session_id)
                || run.platform == nuono_app::agent::ACCOUNT_CHAT_PLATFORM =>
        {
            return ApiResponse::not_found("任务不存在");
        }
        Ok(_) => {}
        Err(error) => return ApiResponse::from_err(&error),
    }
    match state
        .manager
        .event_kernel
        .agent_runtime
        .steer_run(&id, &body.text)
        .await
    {
        Ok(()) => ApiResponse::ok(serde_json::json!({"queued":true})),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(post, path="/runs/{id}/cancel", operation_id="ai_cancel_agent_run", tag="ai", params(("id"=String, Path)),
    responses((status=200, body=ApiResponse<serde_json::Value>)))]
pub(super) async fn cancel_agent_run(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<serde_json::Value> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match state
        .manager
        .event_kernel
        .agent_runtime
        .cancel_run(&id)
        .await
    {
        Ok(()) => ApiResponse::ok(serde_json::json!({"cancel_requested":true})),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(post, path="/runs/{id}/collect", operation_id="ai_collect_agent_children", tag="ai", params(("id"=String, Path)),
    responses((status=200, body=ApiResponse<serde_json::Value>)))]
pub(super) async fn collect_agent_children(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<serde_json::Value> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match state
        .manager
        .event_kernel
        .agent_runtime
        .collect_children(&id)
        .await
    {
        Ok(reply) => {
            if let Some(reply) = &reply
                && let Err(error) = state.manager.event_kernel.send_agent_run_reply(reply).await
            {
                tracing::warn!(%error, run_id=%reply.run_id, "Parent continued; reply delivery failed, result retained in run");
            }
            ApiResponse::ok(
                serde_json::json!({"resumed":reply.is_some(), "run_id":reply.as_ref().map(|run| &run.run_id)}),
            )
        }
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(post, path="/tool-approvals/{id}/resume", operation_id="ai_resume_agent_approval", tag="ai", params(("id"=String, Path)),
    responses((status=200, body=ApiResponse<serde_json::Value>)))]
pub(super) async fn resume_agent_approval(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<serde_json::Value> {
    if let Err(error) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(error);
    }
    match resume_agent_reply(&state, id).await {
        Ok(reply) => ApiResponse::ok(
            serde_json::json!({"resumed":reply.is_some(), "run_id":reply.as_ref().map(|r| &r.run_id)}),
        ),
        Err(error) => ApiResponse::from_err(&error),
    }
}
