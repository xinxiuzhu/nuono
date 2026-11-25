use axum::extract::State;
use std::collections::HashMap;

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;

use super::{
    PluginServiceConfig, PluginServicePatch, ensure_plugin_accessible, expected_revision,
    plugin_service_config,
};

#[utoipa::path(
    post,
    path = "/{name}/config",
    operation_id = "plugins_save_config",
    tag = "plugins",
    request_body = HashMap<String, nuono_core::config::ConfigValue>,
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<crate::api::docs::Empty>))
)]
pub async fn save_config(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
    headers: axum::http::HeaderMap,
    Json(values): Json<HashMap<String, nuono_core::config::ConfigValue>>,
) -> crate::api::ApiReply<()> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    match state
        .plugin_config
        .set_values_as(
            caller.user_id,
            &name,
            values,
            match expected_revision(&headers) {
                Some(value) => value,
                None => return ApiResponse::bad_request("保存需要 If-Match 配置版本"),
            },
        )
        .await
    {
        Ok(()) => {
            // 全局值是按群解析的底座，且只有未被该群覆盖的字段会跟着变 —— 受影响的群集合
            // 正是「所有群」，所以按插件名整片清而不是逐群算。
            match state.manager.plugin_host.refresh_plugin_config(&name).await {
                Ok(()) => ApiResponse::ok(()),
                Err(error) => ApiResponse::from_err(&error),
            }
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(delete, path = "/{name}/config", operation_id = "plugins_reset_config_fields",
    tag = "plugins", request_body = Vec<String>, params(("name" = String, Path)),
    responses((status = 200, description = "删除指定显式覆盖，恢复继承默认值", body = ApiResponse<crate::api::docs::Empty>)))]
pub async fn reset_config_fields(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
    headers: axum::http::HeaderMap,
    Json(fields): Json<Vec<String>>,
) -> crate::api::ApiReply<()> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    let Some(expected) = expected_revision(&headers) else {
        return ApiResponse::bad_request("恢复继承需要 If-Match 配置版本");
    };
    match state
        .plugin_config
        .reset_fields_as(Some(caller.user_id), &name, &fields, expected)
        .await
    {
        Ok(()) => match state.manager.plugin_host.refresh_plugin_config(&name).await {
            Ok(()) => ApiResponse::ok(()),
            Err(error) => ApiResponse::from_err(&error),
        },
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/{name}/service",
    operation_id = "plugins_save_service",
    tag = "plugins",
    request_body = PluginServicePatch,
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<PluginServiceConfig>))
)]
pub async fn save_service(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PluginServicePatch>,
) -> crate::api::ApiReply<PluginServiceConfig> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    let values = body.into_values();

    match state
        .plugin_config
        .set_values_as(
            caller.user_id,
            &name,
            values,
            match expected_revision(&headers) {
                Some(value) => value,
                None => return ApiResponse::bad_request("保存需要 If-Match 配置版本"),
            },
        )
        .await
    {
        Ok(()) => match state.manager.plugin_host.refresh_plugin_config(&name).await {
            Ok(()) => ApiResponse::ok(plugin_service_config(&state, &name).await),
            Err(error) => ApiResponse::from_err(&error),
        },
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/{name}/config/status",
    operation_id = "plugins_config_status",
    tag = "plugins",
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<nuono_core::config::PluginConfigStatus>))
)]
pub async fn config_status(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<nuono_core::config::PluginConfigStatus> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    ApiResponse::ok(state.plugin_config.status(&name).await)
}
