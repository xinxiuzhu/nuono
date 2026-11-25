//! 源码插件商店目录查询与插件安装/更新任务管理的 HTTP 接口层。
mod git;
mod jobs;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use crate::source_operations::{self, OperationContext, plugin_target, store_catalog};
pub use crate::source_operations::{StoreCatalog, StorePlugin, StoreSettingsBody};
use crate::{AppState, api::ApiResponse};
use axum::extract::State;
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

fn context(state: &AppState) -> OperationContext {
    OperationContext {
        bot_instances: state.bot_instances.clone(),
        data: state.paths.data.clone(),
        config: state.config.clone(),
        db: state.db.clone(),
        plugin_host: state.manager.plugin_host.clone(),
        framework_control: state.manager.framework_control.clone(),
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(catalog))
        .routes(routes!(refresh))
        .routes(routes!(settings))
        .routes(routes!(save_settings))
        .merge(jobs::router())
        .merge(git::router())
        .routes(routes!(check_installed))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::plugin_store::InstallBody)]
pub struct InstallBody {
    pub repo_url: Option<String>,
}

#[utoipa::path(
    get, path = "/store/settings", operation_id = "plugin_store_settings", tag = "plugin_store",
    responses((status = 200, body = ApiResponse<StoreSettingsBody>))
)]
async fn settings(State(state): State<AppState>) -> crate::api::ApiReply<StoreSettingsBody> {
    let cfg = state.config.snapshot().await;
    ApiResponse::ok(StoreSettingsBody {
        index_url: cfg.plugin_store.index_url.clone().unwrap_or_default(),
        signing_public_key: cfg
            .plugin_store
            .signing_public_key
            .clone()
            .unwrap_or_default(),
        require_signature: cfg.plugin_store.require_signature,
    })
}

#[utoipa::path(
    get,
    path = "/store/list",
    operation_id = "plugin_store_list",
    tag = "plugin_store",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<StorePlugin>>))
)]
async fn list(State(state): State<AppState>) -> crate::api::ApiReply<Vec<StorePlugin>> {
    let cfg = state.config.snapshot().await;
    match store_catalog(&cfg.plugin_store, false).await {
        Ok(view) => ApiResponse::ok(view.plugins),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}

#[utoipa::path(
    get, path = "/store/catalog", operation_id = "plugin_store_catalog", tag = "plugin_store",
    responses((status = 200, body = ApiResponse<StoreCatalog>))
)]
async fn catalog(State(state): State<AppState>) -> crate::api::ApiReply<StoreCatalog> {
    let cfg = state.config.snapshot().await;
    match store_catalog(&cfg.plugin_store, false).await {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}

#[utoipa::path(
    post, path = "/store/refresh", operation_id = "plugin_store_refresh", tag = "plugin_store",
    responses((status = 200, body = ApiResponse<StoreCatalog>))
)]
async fn refresh(State(state): State<AppState>) -> crate::api::ApiReply<StoreCatalog> {
    let cfg = state.config.snapshot().await;
    match store_catalog(&cfg.plugin_store, true).await {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}

#[utoipa::path(
    post, path = "/store/settings", operation_id = "plugin_store_save_settings", tag = "plugin_store",
    request_body = StoreSettingsBody,
    responses((status = 200, body = ApiResponse<super::docs::Empty>))
)]
async fn save_settings(
    State(state): State<AppState>,
    Json(body): Json<StoreSettingsBody>,
) -> crate::api::ApiReply<()> {
    match source_operations::save_settings(&state.config, body).await {
        Ok(()) => ApiResponse::ok(()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get,
    path = "/store/check/{id}",
    operation_id = "plugin_store_check_installed",
    tag = "plugin_store",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<bool>))
)]
async fn check_installed(Path(id): Path<String>) -> crate::api::ApiReply<bool> {
    match plugin_target(&id) {
        Ok(target) => ApiResponse::ok(target.exists()),
        Err(error) => ApiResponse::bad_request(error),
    }
}
