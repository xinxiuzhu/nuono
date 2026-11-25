use crate::api::input::ApiJson as Json;

use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::adapters::CreateAdapterRequest)]
pub struct CreateAdapterRequest {
    pub id: String,
    pub adapter_type: String,
    pub config: serde_json::Value,
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_adapters))
        .routes(routes!(create_adapter))
        .routes(routes!(start_adapter))
        .routes(routes!(stop_adapter))
        .routes(routes!(delete_adapter))
}

#[utoipa::path(
    get,
    path = "/adapters",
    operation_id = "adapters_list_adapters",
    tag = "adapters",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<super::docs::AdapterInfoSchema>>))
)]
async fn list_adapters(
    state: axum::extract::State<AppState>,
) -> crate::api::ApiReply<Vec<nuono_core::adapter::AdapterInfo>> {
    let adapters = state.adapter_manager.get_all().await;
    ApiResponse::ok(adapters)
}

#[utoipa::path(
    post,
    path = "/adapters",
    operation_id = "adapters_create_adapter",
    tag = "adapters",
    request_body = CreateAdapterRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn create_adapter(
    state: axum::extract::State<AppState>,
    Json(req): Json<CreateAdapterRequest>,
) -> crate::api::ApiReply<String> {
    match state
        .bot_instances
        .create(crate::bot_instances::CreateBotInstanceRequest {
            name: req.id,
            adapter_type: req.adapter_type,
            enabled: true,
            config: req.config,
            ..Default::default()
        })
        .await
    {
        Ok(instance) => ApiResponse::ok(instance.id),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/adapters/{id}/start",
    operation_id = "adapters_start_adapter",
    tag = "adapters",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn start_adapter(
    state: axum::extract::State<AppState>,
    crate::api::input::ApiPath(id): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<String> {
    match state.bot_instances.start(&id).await {
        Ok(_) => ApiResponse::ok(format!("Adapter {} started", id)),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    post,
    path = "/adapters/{id}/stop",
    operation_id = "adapters_stop_adapter",
    tag = "adapters",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn stop_adapter(
    state: axum::extract::State<AppState>,
    crate::api::input::ApiPath(id): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<String> {
    match state.bot_instances.stop(&id).await {
        Ok(_) => ApiResponse::ok(format!("Adapter {} stopped", id)),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/adapters/{id}",
    operation_id = "adapters_delete_adapter",
    tag = "adapters",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn delete_adapter(
    state: axum::extract::State<AppState>,
    crate::api::input::ApiPath(id): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<String> {
    match state.bot_instances.delete(&id).await {
        Ok(_) => ApiResponse::ok(format!("Adapter {} removed", id)),
        Err(e) => ApiResponse::from_err(&e),
    }
}
