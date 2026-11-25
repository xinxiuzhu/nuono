//! 网页控制复用框架的优雅停止、重启与维护锁。
use axum::extract::State;
use nuono_core::core_commands::RestartService;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    api::{ApiReply, ApiResponse, input::ApiJson},
    maintenance::ServerRestartService,
};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(shutdown))
        .routes(routes!(restart))
}

#[derive(Deserialize, ToSchema)]
struct ServiceControlRequest {
    confirmation: String,
}

#[utoipa::path(post, path = "/shutdown", operation_id = "system_shutdown", tag = "system",
    request_body = ServiceControlRequest,
    responses((status = 200, description = "已提交 Nuono 优雅停止请求", body = ApiResponse<String>),
        (status = 400, description = "确认内容无效"), (status = 409, description = "正在执行维护操作")))]
async fn shutdown(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ServiceControlRequest>,
) -> ApiReply<String> {
    if body.confirmation != "shutdown" {
        return ApiResponse::bad_request("请确认关闭 Nuono 服务");
    }
    let service = ServerRestartService::new(&state.paths, state.manager.framework_control.clone());
    match service.shutdown("dashboard shutdown").await {
        Ok(()) => {
            tracing::info!("管理网页已请求关闭 Nuono 服务");
            ApiResponse::ok("关机请求已提交".to_string())
        }
        Err(error) => ApiResponse::conflict(error),
    }
}

#[utoipa::path(post, path = "/restart", operation_id = "system_restart", tag = "system",
    request_body = ServiceControlRequest,
    responses((status = 200, description = "已提交 Nuono 重启请求", body = ApiResponse<String>),
        (status = 400, description = "确认内容无效"), (status = 409, description = "无法自动重启或正在维护")))]
async fn restart(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ServiceControlRequest>,
) -> ApiReply<String> {
    if body.confirmation != "restart" {
        return ApiResponse::bad_request("请确认重启 Nuono 服务");
    }
    let service = ServerRestartService::new(&state.paths, state.manager.framework_control.clone());
    let prepared = match service.prepare_restart("dashboard restart", None).await {
        Ok(prepared) => prepared,
        Err(error) => return ApiResponse::conflict(error),
    };
    match prepared.commit().await {
        Ok(()) => {
            tracing::info!("管理网页已请求重启 Nuono 服务");
            ApiResponse::ok("重启请求已提交".to_string())
        }
        Err(error) => ApiResponse::internal(error),
    }
}
