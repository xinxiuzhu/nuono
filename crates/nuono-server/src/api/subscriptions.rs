use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use axum::extract::State;
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(create))
        .routes(routes!(toggle))
        .routes(routes!(remove))
}

/// 列出框架内置调度器的通用主题推送订阅（插件级群定时推送订阅请使用 `GET /api/groups/{group_id}/subscriptions`）。
#[utoipa::path(
    get,
    path = "/",
    operation_id = "subscriptions_list",
    tag = "subscriptions",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::pubsub::subscriptions::Subscription>>))
)]
async fn list(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<nuono_core::service::pubsub::subscriptions::Subscription>> {
    ApiResponse::from_result(nuono_core::service::pubsub::subscriptions::list(&state.db).await)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::subscriptions::CreateBody)]
struct CreateBody {
    subscriber_id: String,
    topic: String,
    target_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/",
    operation_id = "subscriptions_create",
    tag = "subscriptions",
    request_body = CreateBody,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<nuono_core::service::pubsub::subscriptions::Subscription>))
)]
async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> crate::api::ApiReply<nuono_core::service::pubsub::subscriptions::Subscription> {
    match nuono_core::service::pubsub::subscriptions::create(
        &state.db,
        &body.subscriber_id,
        &body.topic,
        body.target_id.as_deref(),
    )
    .await
    {
        Ok(sub) => ApiResponse::ok(sub),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::subscriptions::ToggleBody)]
struct ToggleBody {
    enabled: bool,
}

#[utoipa::path(
    post,
    path = "/{id}/toggle",
    operation_id = "subscriptions_toggle",
    tag = "subscriptions",
    request_body = ToggleBody,
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn toggle(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ToggleBody>,
) -> crate::api::ApiReply<()> {
    match nuono_core::service::pubsub::subscriptions::toggle(&state.db, id, body.enabled).await {
        Ok(()) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/{id}",
    operation_id = "subscriptions_remove",
    tag = "subscriptions",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> crate::api::ApiReply<()> {
    match nuono_core::service::pubsub::subscriptions::remove(&state.db, id).await {
        Ok(()) => ApiResponse::ok(()),
        Err(e) => ApiResponse::from_err(&e),
    }
}
