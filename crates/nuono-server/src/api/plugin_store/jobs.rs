//! 持久化源码插件操作任务（安装、更新、卸载、重试、重启）的 HTTP 接口。
use super::{
    ApiResponse, AppState, InstallBody, Json, LinkInstallBody, OpenApiRouter, Path, State, context,
    routes,
};
use crate::source_operations::jobs::{self, StoreJob, StoreJobs};
use serde::Deserialize;

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_jobs))
        .routes(routes!(install_from_repo))
        .routes(routes!(install))
        .routes(routes!(update))
        .routes(routes!(uninstall))
        .routes(routes!(retry))
        .routes(routes!(restart))
}
#[derive(Deserialize, Default)]
struct JobQuery {
    log_id: Option<String>,
}
#[utoipa::path(get, path="/store/jobs", operation_id="plugin_store_jobs", tag="plugin_store", params(("log_id"=Option<String>,Query,description="只读取这一项任务的日志，最多256KiB")), responses((status=200, body=ApiResponse<StoreJobs>)))]
async fn list_jobs(
    State(state): State<AppState>,
    crate::api::input::ApiQuery(query): crate::api::input::ApiQuery<JobQuery>,
) -> crate::api::ApiReply<StoreJobs> {
    match jobs::list_jobs(&context(&state), query.log_id.as_deref()).await {
        Ok(jobs) => ApiResponse::ok(jobs),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post, path="/store/install", operation_id="plugin_store_install_from_repo", tag="plugin_store", description="从 Git 仓库链接安装扩展。类型由仓库 Cargo.toml 自动识别（适配器自动装入 adapters/）；扩展 ID 为选填，留空时自动从仓库地址推导（如 nuono-adapter-qq → qq）。", request_body=LinkInstallBody, responses((status=200, body=ApiResponse<crate::source_schemas::StoreJobSchema>)))]
async fn install_from_repo(
    State(state): State<AppState>,
    Json(body): Json<LinkInstallBody>,
) -> crate::api::ApiReply<StoreJob> {
    match jobs::submit_link_install(context(&state), body.repo_url, body.id).await {
        Ok(job) => ApiResponse::ok(job),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post, path="/store/install/{id}", operation_id="plugin_store_install", tag="plugin_store", description="从插件商店目录安装指定扩展（商店条目自带类型与 ID，适配器路径带 adapter: 前缀）。", request_body=InstallBody, params(("id"=String,Path)), responses((status=200, body=ApiResponse<crate::source_schemas::StoreJobSchema>)))]
async fn install(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(_body): Json<InstallBody>,
) -> crate::api::ApiReply<StoreJob> {
    match jobs::submit(context(&state), id, "install", None, None, None).await {
        Ok(job) => ApiResponse::ok(job),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post, path="/store/update/{id}", operation_id="plugin_store_update", tag="plugin_store", params(("id"=String,Path)), responses((status=200, body=ApiResponse<crate::source_schemas::StoreJobSchema>)))]
async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<StoreJob> {
    match jobs::submit(context(&state), id, "update", None, None, None).await {
        Ok(job) => ApiResponse::ok(job),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(delete, path="/store/uninstall/{id}", operation_id="plugin_store_uninstall", tag="plugin_store", params(("id"=String,Path)), responses((status=200, body=ApiResponse<crate::source_schemas::StoreJobSchema>)))]
async fn uninstall(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<StoreJob> {
    match jobs::submit(context(&state), id, "uninstall", None, None, None).await {
        Ok(job) => ApiResponse::ok(job),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post, path="/store/jobs/{id}/retry", operation_id="plugin_store_job_retry", tag="plugin_store", params(("id"=String,Path)), responses((status=200, body=ApiResponse<crate::source_schemas::StoreJobSchema>)))]
async fn retry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<StoreJob> {
    match jobs::retry(context(&state), id).await {
        Ok(value) => ApiResponse::ok(value),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post, path="/store/jobs/{id}/restart", operation_id="plugin_store_job_restart", tag="plugin_store", params(("id"=String,Path)), responses((status=200, body=ApiResponse<String>)))]
async fn restart(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<String> {
    match jobs::restart(context(&state), id).await {
        Ok(value) => ApiResponse::ok(value),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
