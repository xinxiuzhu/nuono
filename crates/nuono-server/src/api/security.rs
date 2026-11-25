use crate::api::input::ApiJson as Json;
use axum::extract::State;
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

#[utoipa::path(
    get,
    path = "/blacklists",
    operation_id = "security_list_blacklists",
    tag = "security",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::security::BlacklistRecord>>))
)]
pub async fn list_blacklists(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<nuono_core::service::security::BlacklistRecord>> {
    ApiResponse::from_result(nuono_core::service::security::get_blacklists(&state.db).await)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::security::AddBlacklistBody)]
pub struct AddBlacklistBody {
    target_type: String,
    target_id: String,
    reason: Option<String>,
}

#[utoipa::path(
    post,
    path = "/blacklists",
    operation_id = "security_add_blacklist",
    tag = "security",
    request_body = AddBlacklistBody,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn add_blacklist(
    State(state): State<AppState>,
    Json(body): Json<AddBlacklistBody>,
) -> crate::api::ApiReply<String> {
    match nuono_core::service::security::add_blacklist(
        &state.db,
        &body.target_type,
        &body.target_id,
        body.reason.as_deref(),
    )
    .await
    {
        Ok(id) => ApiResponse::ok(id.to_string()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/blacklists/{id}",
    operation_id = "security_delete_blacklist",
    tag = "security",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
pub async fn delete_blacklist(
    State(state): State<AppState>,
    crate::api::input::ApiPath(id): crate::api::input::ApiPath<i64>,
) -> crate::api::ApiReply<()> {
    match nuono_core::service::security::delete_blacklist(&state.db, id).await {
        Ok(()) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/filter-rules",
    operation_id = "security_list_filter_rules",
    tag = "security",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::security::FilterRuleRecord>>))
)]
pub async fn list_filter_rules(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<nuono_core::service::security::FilterRuleRecord>> {
    ApiResponse::from_result(nuono_core::service::security::get_filter_rules(&state.db).await)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::security::AddFilterRuleBody)]
pub struct AddFilterRuleBody {
    filter_type: String,
    pattern: String,
    match_mode: Option<String>,
    target_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/filter-rules",
    operation_id = "security_add_filter_rule",
    tag = "security",
    request_body = AddFilterRuleBody,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn add_filter_rule(
    State(state): State<AppState>,
    Json(body): Json<AddFilterRuleBody>,
) -> crate::api::ApiReply<String> {
    match nuono_core::service::security::add_filter_rule(
        &state.db,
        &body.filter_type,
        &body.pattern,
        body.match_mode.as_deref(),
        body.target_id.as_deref(),
    )
    .await
    {
        Ok(id) => ApiResponse::ok(id.to_string()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::security::ToggleFilterBody)]
pub struct ToggleFilterBody {
    enabled: bool,
}

#[utoipa::path(
    post,
    path = "/filter-rules/{id}/toggle",
    operation_id = "security_toggle_filter_rule",
    tag = "security",
    request_body = ToggleFilterBody,
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
pub async fn toggle_filter_rule(
    State(state): State<AppState>,
    crate::api::input::ApiPath(id): crate::api::input::ApiPath<i64>,
    Json(body): Json<ToggleFilterBody>,
) -> crate::api::ApiReply<()> {
    match nuono_core::service::security::toggle_filter_rule(&state.db, id, body.enabled).await {
        Ok(()) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/filter-rules/{id}",
    operation_id = "security_delete_filter_rule",
    tag = "security",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
pub async fn delete_filter_rule(
    State(state): State<AppState>,
    crate::api::input::ApiPath(id): crate::api::input::ApiPath<i64>,
) -> crate::api::ApiReply<()> {
    match nuono_core::service::security::delete_filter_rule(&state.db, id).await {
        Ok(()) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_blacklists, add_blacklist))
        .routes(routes!(delete_blacklist))
        .routes(routes!(list_filter_rules, add_filter_rule))
        .routes(routes!(delete_filter_rule))
        .routes(routes!(toggle_filter_rule))
}
