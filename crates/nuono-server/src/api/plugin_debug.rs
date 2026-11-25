use axum::extract::State;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

#[utoipa::path(
    get,
    path = "/plugin-kv",
    operation_id = "plugin_debug_list_plugin_kv",
    tag = "plugin_debug",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::plugin_debug::PluginKvRecord>>))
)]
pub async fn list_plugin_kv(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<nuono_core::service::plugin_debug::PluginKvRecord>> {
    match nuono_core::service::plugin_debug::get_plugin_kv(&state.db, 200).await {
        Ok(rows) => ApiResponse::ok(rows),
        Err(e) => ApiResponse::from_err(&e),
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list_plugin_kv))
}
