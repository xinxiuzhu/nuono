//! Agent 记忆管理、图谱浏览、维护与过期清理接口。

use axum::extract::State;
use serde::{Deserialize, Serialize};

use super::config::normalize_policy_list;
use super::runs::run_operator;
use super::{private_scope, public_memory};
use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};
use nuono_core::audit;
use nuono_core::db::{AgentMemoryRecord, AgentMemoryUpdateRecord};

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MemoryGraphQuery {
    pub scope_type: String,
    pub scope_key: String,
}

#[utoipa::path(get, path = "/memories/graph", operation_id = "ai_memory_graph", tag = "ai", params(MemoryGraphQuery), responses((status = 200, body = ApiResponse<serde_json::Value>)))]
pub(super) async fn get_memory_graph(
    State(state): State<AppState>,
    Query(query): Query<MemoryGraphQuery>,
) -> crate::api::ApiReply<serde_json::Value> {
    if private_scope(query.scope_key.trim()) {
        return ApiResponse::not_found("记忆不存在");
    }
    let Some(scope_type) = normalize_memory_scope(&query.scope_type) else {
        return ApiResponse::bad_request("记忆范围无效");
    };
    let scope = nuono_app::agent::memory::MemoryScope {
        scope_type,
        scope_key: query.scope_key,
    };
    match state.db.memory_graph(&scope).await {
        Ok(Some(graph)) => {
            let source = match state
                .db
                .list_agent_memories(
                    Some(&scope.scope_type),
                    Some(&scope.scope_key),
                    Some(true),
                    true,
                    graph.source_limit as u64,
                )
                .await
            {
                Ok(rows) => rows.into_iter().map(Into::into).collect::<Vec<_>>(),
                Err(e) => return ApiResponse::from_err(&e),
            };
            ApiResponse::ok(
                serde_json::json!({"record":graph,"stale":nuono_app::agent::memory::fingerprint(&source) != graph.source_fingerprint}),
            )
        }
        Ok(None) => ApiResponse::ok(serde_json::json!({"record":null,"stale":false})),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(post, path = "/memories/maintain", operation_id = "ai_memory_maintain", tag = "ai", responses((status = 200, body = ApiResponse<serde_json::Value>)))]
pub(super) async fn maintain_memories(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_app::auth::CallerIdentity>>,
) -> crate::api::ApiReply<serde_json::Value> {
    if let Err(e) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(e);
    }
    match state
        .manager
        .event_kernel
        .agent_runtime
        .maintain_memories(false)
        .await
    {
        Ok(result) => {
            ApiResponse::ok(serde_json::to_value(result).expect("maintenance result serializes"))
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PinMemoryPayload {
    pub pinned: bool,
}
#[utoipa::path(patch, path = "/memories/{id}/pin", operation_id = "ai_memory_pin", tag = "ai", params(("id"=String, Path)), request_body = PinMemoryPayload, responses((status = 200, body = ApiResponse<serde_json::Value>)))]
pub(super) async fn pin_memory(
    State(state): State<AppState>,
    caller: Option<axum::extract::Extension<nuono_core::auth::CallerIdentity>>,
    Path(id): Path<i64>,
    Json(body): Json<PinMemoryPayload>,
) -> crate::api::ApiReply<serde_json::Value> {
    if let Err(e) = run_operator(&state, caller).await {
        return ApiResponse::forbidden(e);
    }
    match public_memory(&state, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return ApiResponse::not_found("记忆不存在"),
        Err(error) => return ApiResponse::from_err(&error),
    }
    match state.db.pin_memory(id, body.pinned).await {
        Ok(Some(profile)) => {
            ApiResponse::ok(serde_json::to_value(profile).expect("memory profile serializes"))
        }
        Ok(None) => ApiResponse::not_found("记忆不存在"),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::ai::MemoryQuery)]
#[into_params(parameter_in = Query)]
pub struct MemoryQuery {
    pub scope_type: Option<String>,
    pub scope_key: Option<String>,
    pub enabled: Option<bool>,
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::MemoryResponse)]
pub struct MemoryResponse {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub scope_type: String,
    pub scope_key: String,
    pub memory_key: String,
    pub content: String,
    pub tags: Vec<String>,
    pub pinned: bool,
    pub strength: f64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub recall_count: i64,
    pub enabled: bool,
    pub expires_at: Option<String>,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::ai::CreateMemoryPayload)]
pub struct CreateMemoryPayload {
    pub scope_type: String,
    pub scope_key: String,
    pub memory_key: String,
    pub content: String,
    pub tags: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub expires_at: Option<String>,
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::ai::UpdateMemoryPayload)]
pub struct UpdateMemoryPayload {
    pub scope_type: Option<String>,
    pub scope_key: Option<String>,
    pub memory_key: Option<String>,
    pub content: Option<String>,
    pub tags: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub expires_at: Option<String>,
    pub actor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/memories",
    operation_id = "ai_list_memories",
    tag = "ai",
    params(MemoryQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<MemoryResponse>>))
)]
pub(super) async fn list_memories(
    State(state): State<AppState>,
    Query(query): Query<MemoryQuery>,
) -> crate::api::ApiReply<Vec<MemoryResponse>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let scope_type = query.scope_type.as_deref().and_then(normalize_memory_scope);
    let scope_key = query
        .scope_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let rows = match state
        .db
        .list_agent_memories(
            scope_type.as_deref(),
            scope_key,
            query.enabled,
            false,
            limit,
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => return ApiResponse::from_err(&e),
    };

    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| !private_scope(&row.scope_key))
        .collect();
    let cfg = state.config.snapshot().await;
    let profiles = match state
        .db
        .memory_profiles(&rows.iter().map(|m| m.id).collect::<Vec<_>>())
        .await
    {
        Ok(profiles) => profiles
            .into_iter()
            .map(|p| (p.memory_id, p))
            .collect::<std::collections::HashMap<_, _>>(),
        Err(e) => return ApiResponse::from_err(&e),
    };
    ApiResponse::ok(
        rows.into_iter()
            .map(|row| {
                let memory: nuono_app::agent::store::MemoryEntry = row.clone().into();
                let profile = profiles.get(&row.id).cloned().unwrap_or_else(|| {
                    nuono_app::agent::memory::MemoryProfile::for_memory(&memory)
                });
                let mut response = memory_response(row);
                response.pinned = profile.pinned;
                response.recall_count = profile.recall_count;
                response.strength = nuono_app::agent::memory::strength(
                    &memory,
                    &profile,
                    &cfg.ai.agent_memory_policy,
                    chrono::Utc::now(),
                );
                response
            })
            .collect(),
    )
}

#[utoipa::path(
    get,
    path = "/memories/{id}",
    operation_id = "ai_get_memory",
    tag = "ai",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<MemoryResponse>))
)]
pub(super) async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<MemoryResponse> {
    match public_memory(&state, id).await {
        Ok(Some(row)) => match memory_response_current(&state, row).await {
            Ok(response) => ApiResponse::ok(response),
            Err(e) => ApiResponse::from_err(&e),
        },
        Ok(None) => ApiResponse::not_found(format!("记忆 {} 不存在", id)),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    post,
    path = "/memories",
    operation_id = "ai_create_memory",
    tag = "ai",
    request_body = CreateMemoryPayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<MemoryResponse>))
)]
pub(super) async fn create_memory(
    State(state): State<AppState>,
    Json(body): Json<CreateMemoryPayload>,
) -> crate::api::ApiReply<MemoryResponse> {
    let Some(scope_type) = normalize_memory_scope(&body.scope_type) else {
        return ApiResponse::bad_request("scope_type 只允许 session/user/group/global".to_string());
    };
    let scope_key = body.scope_key.trim().to_string();
    if private_scope(&scope_key) {
        return ApiResponse::not_found("记忆不存在");
    }
    let memory_key = body.memory_key.trim().to_string();
    let content = body.content.trim().to_string();
    if scope_key.is_empty() || memory_key.is_empty() || content.is_empty() {
        return ApiResponse::bad_request("scope_key、memory_key 和 content 不能为空".to_string());
    }

    match state
        .db
        .upsert_agent_memory(AgentMemoryRecord {
            scope_type,
            scope_key,
            memory_key,
            content,
            tags_json: memory_tags_json(body.tags),
            enabled: body.enabled.unwrap_or(true),
            expires_at: normalize_optional_string(body.expires_at)
                .and_then(|s| nuono_core::clock::parse_db_ts(&s)),
            actor: normalize_optional_string(body.actor),
        })
        .await
    {
        Ok((row, created)) => {
            let (event_type, action) = if created {
                ("agent.memory_created", "created")
            } else {
                ("agent.memory_updated", "upsert_updated")
            };
            record_memory_audit(&state, event_type, &row, action).await;
            match memory_response_current(&state, row).await {
                Ok(response) => ApiResponse::ok(response),
                Err(e) => ApiResponse::from_err(&e),
            }
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    put,
    path = "/memories/{id}",
    operation_id = "ai_update_memory",
    tag = "ai",
    request_body = UpdateMemoryPayload,
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<MemoryResponse>))
)]
pub(super) async fn update_memory(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMemoryPayload>,
) -> crate::api::ApiReply<MemoryResponse> {
    match public_memory(&state, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return ApiResponse::not_found("记忆不存在"),
        Err(error) => return ApiResponse::from_err(&error),
    }
    if body
        .scope_key
        .as_deref()
        .is_some_and(|key| private_scope(key.trim()))
    {
        return ApiResponse::not_found("记忆不存在");
    }
    let scope_type = match body.scope_type {
        Some(value) => match normalize_memory_scope(&value) {
            Some(scope) => Some(scope),
            None => {
                return ApiResponse::bad_request(
                    "scope_type 只允许 session/user/group/global".to_string(),
                );
            }
        },
        None => None,
    };
    let scope_key = body
        .scope_key
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let memory_key = body
        .memory_key
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let content = body
        .content
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    match state
        .db
        .update_agent_memory(
            id,
            AgentMemoryUpdateRecord {
                scope_type,
                scope_key,
                memory_key,
                content,
                tags_json: body.tags.map(normalized_tags_json),
                enabled: body.enabled,
                expires_at: body
                    .expires_at
                    .map(|value| normalize_optional_string(Some(value)))
                    .map(|opt| opt.and_then(|s| nuono_core::clock::parse_db_ts(&s))),
                actor: normalize_optional_string(body.actor),
            },
        )
        .await
    {
        Ok(Some(row)) => {
            record_memory_audit(&state, "agent.memory_updated", &row, "updated").await;
            match memory_response_current(&state, row).await {
                Ok(response) => ApiResponse::ok(response),
                Err(e) => ApiResponse::from_err(&e),
            }
        }
        Ok(None) => ApiResponse::not_found(format!("记忆 {} 不存在", id)),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/memories/{id}",
    operation_id = "ai_delete_memory",
    tag = "ai",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::Empty>))
)]
pub(super) async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<()> {
    let before = match public_memory(&state, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return ApiResponse::not_found("记忆不存在"),
        Err(error) => return ApiResponse::from_err(&error),
    };
    match state.db.delete_agent_memory(id).await {
        Ok(true) => {
            record_memory_audit(&state, "agent.memory_deleted", &before, "deleted").await;
            ApiResponse::ok(())
        }
        Ok(false) => ApiResponse::not_found(format!("记忆 {} 不存在", id)),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::ai::PurgeExpiredMemoriesResponse)]
pub struct PurgeExpiredMemoriesResponse {
    pub deleted: u64,
}

#[utoipa::path(
    post,
    path = "/memories/expired/purge",
    operation_id = "ai_purge_expired_memories",
    tag = "ai",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<PurgeExpiredMemoriesResponse>))
)]
pub(super) async fn purge_expired_memories(
    State(state): State<AppState>,
) -> crate::api::ApiReply<PurgeExpiredMemoriesResponse> {
    match state.db.delete_expired_agent_memories().await {
        Ok(deleted) => {
            let data = serde_json::json!({
                "action": "expired_purged",
                "deleted": deleted,
            });
            if let Err(e) =
                audit::record_event(&state.db, "agent.memory_expired_purged", None, None, data)
                    .await
            {
                tracing::warn!(error = %e, "Agent memory expired purge audit record failed");
            }
            ApiResponse::ok(PurgeExpiredMemoriesResponse { deleted })
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

async fn record_memory_audit(
    state: &AppState,
    event_type: &str,
    row: &nuono_core::entity::agent_memories::Model,
    action: &str,
) {
    let actor = row.updated_by.as_deref().or(row.created_by.as_deref());
    let data = serde_json::json!({
        "memory_id": row.id.to_string(),
        "action": action,
        "scope_type": &row.scope_type,
        "scope_key": &row.scope_key,
        "memory_key": &row.memory_key,
        "content_len": row.content.len(),
        "tags": parse_memory_tags(&row.tags_json),
        "enabled": row.enabled,
        "expires_at": &row.expires_at.map(nuono_core::clock::format_wire_ts),
        "actor": actor,
        "created_by": &row.created_by,
        "updated_by": &row.updated_by,
        "created_at": &nuono_core::clock::format_wire_ts(row.created_at),
        "updated_at": &nuono_core::clock::format_wire_ts(row.updated_at),
    });
    if let Err(e) = audit::record_event(&state.db, event_type, None, actor, data).await {
        tracing::warn!(error = %e, memory_id = row.id, event_type, "Agent memory audit record failed");
    }
}

async fn memory_response_current(
    state: &AppState,
    row: nuono_core::entity::agent_memories::Model,
) -> nuono_core::error::Result<MemoryResponse> {
    let memory: nuono_app::agent::store::MemoryEntry = row.clone().into();
    let profile = state
        .db
        .memory_profiles(&[row.id])
        .await?
        .into_iter()
        .next()
        .unwrap_or_else(|| nuono_app::agent::memory::MemoryProfile::for_memory(&memory));
    let cfg = state.config.snapshot().await;
    let mut response = memory_response(row);
    response.pinned = profile.pinned;
    response.recall_count = profile.recall_count;
    response.strength = nuono_app::agent::memory::strength(
        &memory,
        &profile,
        &cfg.ai.agent_memory_policy,
        chrono::Utc::now(),
    );
    Ok(response)
}

fn memory_response(row: nuono_core::entity::agent_memories::Model) -> MemoryResponse {
    let memory: nuono_app::agent::store::MemoryEntry = row.clone().into();
    let profile = nuono_app::agent::memory::MemoryProfile::for_memory(&memory);
    MemoryResponse {
        pinned: profile.pinned,
        strength: 1.0,
        recall_count: 0,
        id: row.id,
        scope_type: row.scope_type,
        scope_key: row.scope_key,
        memory_key: row.memory_key,
        content: row.content,
        tags: parse_memory_tags(&row.tags_json),
        enabled: row.enabled,
        expires_at: row.expires_at.map(nuono_core::clock::format_wire_ts),
        created_by: row.created_by,
        updated_by: row.updated_by,
        created_at: nuono_core::clock::format_wire_ts(row.created_at),
        updated_at: nuono_core::clock::format_wire_ts(row.updated_at),
    }
}

fn normalize_memory_scope(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "session" | "user" | "group" | "global" => Some(value.trim().to_ascii_lowercase()),
        _ => None,
    }
}

fn memory_tags_json(tags: Option<Vec<String>>) -> Option<String> {
    tags.map(normalized_tags_json)
}

fn normalized_tags_json(tags: Vec<String>) -> String {
    let tags = normalize_policy_list(tags);
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

fn parse_memory_tags(tags_json: &Option<String>) -> Vec<String> {
    tags_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}
