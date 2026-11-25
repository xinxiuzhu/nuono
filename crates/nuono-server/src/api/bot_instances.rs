use crate::api::input::{ApiJson as Json, ApiPath as Path};
use axum::extract::State;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    api::ApiResponse,
    bot_instances::{
        BotInstanceTypeInfo, BotInstanceView, CreateBotInstanceRequest, UpdateBotInstanceRequest,
    },
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_types))
        .routes(routes!(list_instances, create_instance))
        .routes(routes!(get_instance, update_instance, delete_instance))
        .routes(routes!(start_instance))
        .routes(routes!(stop_instance))
        .routes(routes!(restart_instance))
}

#[utoipa::path(
    get,
    path = "/bot-instance-types",
    operation_id = "bot_instances_list_types",
    tag = "bot_instances",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<BotInstanceTypeInfo>>))
)]
async fn list_types() -> crate::api::ApiReply<Vec<BotInstanceTypeInfo>> {
    ApiResponse::ok(crate::bot_instances::BotInstanceRuntimeManager::instance_types())
}

#[utoipa::path(
    get,
    path = "/bot-instances",
    operation_id = "bot_instances_list_instances",
    tag = "bot_instances",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<BotInstanceView>>))
)]
async fn list_instances(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<BotInstanceView>> {
    match state.bot_instances.list().await {
        Ok(instances) => ApiResponse::ok(instances),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get,
    path = "/bot-instances/{id}",
    operation_id = "bot_instances_get_instance",
    tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotInstanceView>))
)]
async fn get_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<BotInstanceView> {
    match state.bot_instances.get(&id).await {
        Ok(Some(instance)) => ApiResponse::ok(instance),
        Ok(None) => ApiResponse::not_found(format!("Bot 实例 {id} 不存在")),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/bot-instances",
    operation_id = "bot_instances_create_instance",
    tag = "bot_instances",
    request_body = CreateBotInstanceRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotInstanceView>))
)]
async fn create_instance(
    State(state): State<AppState>,
    Json(request): Json<CreateBotInstanceRequest>,
) -> crate::api::ApiReply<BotInstanceView> {
    match state.bot_instances.create(request).await {
        Ok(instance) => ApiResponse::ok(instance),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    patch,
    path = "/bot-instances/{id}",
    operation_id = "bot_instances_update_instance",
    tag = "bot_instances",
    request_body = UpdateBotInstanceRequest,
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotInstanceView>))
)]
async fn update_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateBotInstanceRequest>,
) -> crate::api::ApiReply<BotInstanceView> {
    match state.bot_instances.update(&id, request).await {
        Ok(instance) => ApiResponse::ok(instance),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/bot-instances/{id}/start",
    operation_id = "bot_instances_start_instance",
    tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotInstanceView>))
)]
async fn start_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<BotInstanceView> {
    match state.bot_instances.start(&id).await {
        Ok(instance) => ApiResponse::ok(instance),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/bot-instances/{id}/stop",
    operation_id = "bot_instances_stop_instance",
    tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotInstanceView>))
)]
async fn stop_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<BotInstanceView> {
    match state.bot_instances.stop(&id).await {
        Ok(instance) => ApiResponse::ok(instance),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/bot-instances/{id}/restart",
    operation_id = "bot_instances_restart_instance",
    tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BotInstanceView>))
)]
async fn restart_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<BotInstanceView> {
    match state.bot_instances.restart(&id).await {
        Ok(instance) => ApiResponse::ok(instance),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    delete,
    path = "/bot-instances/{id}",
    operation_id = "bot_instances_delete_instance",
    tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn delete_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<String> {
    match state.bot_instances.delete(&id).await {
        Ok(()) => ApiResponse::ok(id),
        Err(error) => ApiResponse::from_err(&error),
    }
}
