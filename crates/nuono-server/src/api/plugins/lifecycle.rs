use axum::extract::State;
use nuono_core::core::plugin_host::{PluginDiagnostics, PluginTriggerPolicy};
use serde::Deserialize;

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path};

use super::ensure_plugin_accessible;

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::plugins::ToggleBody)]
pub struct ToggleBody {
    enabled: bool,
    expected_revision: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::plugins::DiagnosticsBody)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBody {
    /// 当前插件的慢调用阈值，单位毫秒。0 关闭慢调用提示，调用统计仍保留。
    #[schema(minimum = 0, maximum = 3600000, example = 30000)]
    pub(super) slow_threshold_ms: u64,
}

/// 保存当前插件的宿主诊断策略；数据库提交成功后立即生效，无需重建插件。
/// 修改阈值清除旧慢调用记录，调用次数、耗时和失败累计保持不变。
#[utoipa::path(
    post,
    path = "/{name}/diagnostics",
    operation_id = "plugins_diagnostics",
    tag = "plugins",
    request_body = DiagnosticsBody,
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<PluginDiagnostics>))
)]
pub async fn save_diagnostics(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    Path(name): Path<String>,
    Json(body): Json<DiagnosticsBody>,
) -> crate::api::ApiReply<PluginDiagnostics> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    match state
        .manager
        .plugin_host
        .set_plugin_slow_threshold(&name, body.slow_threshold_ms)
        .await
    {
        Ok(diagnostics) => ApiResponse::ok(diagnostics),
        Err(error) => ApiResponse::from_err(&error),
    }
}

/// 读取已安装插件当前的宿主触发准入与限流策略。
#[utoipa::path(
    get,
    path = "/{name}/trigger-policy",
    operation_id = "plugins_trigger_policy",
    tag = "plugins",
    params(("name" = String, Path)),
    responses((status = 200, description = "当前插件准入策略", body = ApiResponse<PluginTriggerPolicy>))
)]
pub async fn trigger_policy(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    Path(name): Path<String>,
) -> crate::api::ApiReply<PluginTriggerPolicy> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    ApiResponse::ok(state.manager.plugin_host.plugin_trigger_policy(&name))
}

/// 整体更新插件的四项触发准入限流参数；宿主会在数据库落盘成功后立即在内存中生效新策略。
#[utoipa::path(
    patch,
    path = "/{name}/trigger-policy",
    operation_id = "plugins_save_trigger_policy",
    tag = "plugins",
    request_body = PluginTriggerPolicy,
    params(("name" = String, Path)),
    responses((status = 200, description = "已保存的插件准入策略；请检查 success 与 error", body = ApiResponse<PluginTriggerPolicy>))
)]
pub async fn save_trigger_policy(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    Path(name): Path<String>,
    Json(policy): Json<PluginTriggerPolicy>,
) -> crate::api::ApiReply<PluginTriggerPolicy> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    if let Err(error) = policy.validate() {
        return ApiResponse::from_err(&error);
    }
    match state
        .manager
        .plugin_host
        .set_plugin_trigger_policy(&name, policy)
        .await
    {
        Ok(policy) => ApiResponse::ok(policy),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/{name}/toggle",
    operation_id = "plugins_toggle",
    tag = "plugins",
    request_body = ToggleBody,
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn toggle(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
    Json(body): Json<ToggleBody>,
) -> crate::api::ApiReply<String> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    let expected_revision = match body.expected_revision.parse::<i64>() {
        Ok(value) => value,
        Err(_) => return ApiResponse::bad_request("配置版本无效"),
    };
    match state
        .manager
        .plugin_host
        .save_policy_and_apply(
            caller.user_id,
            &name,
            nuono_core::db::plugin_global_settings::PluginPolicyPatch {
                expected_revision,
                enabled: Some(body.enabled),
                ..Default::default()
            },
        )
        .await
    {
        Ok(()) => ApiResponse::ok(format!(
            "插件 {} 已{}",
            name,
            if body.enabled { "启用" } else { "禁用" }
        )),
        Err(e) => ApiResponse::from_err(&e),
    }
}

/// 重建已编译插件实例并恢复启用状态；不读取或编译新源码。
/// 更新插件代码后须重新构建应用并重启程序。
#[utoipa::path(
    post,
    path = "/{name}/reload",
    operation_id = "plugins_reload",
    tag = "plugins",
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn reload(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<String> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    match state.manager.plugin_host.reload_plugin(&name).await {
        Ok(()) => ApiResponse::ok(format!(
            "插件 {name} 实例已重建；代码更新需统一构建应用并重启"
        )),
        Err(e) => ApiResponse::from_err(&e),
    }
}
