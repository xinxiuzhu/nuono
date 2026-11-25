//! 群级插件配置覆盖、定时推送订阅管理及多来源授权接口。
use crate::{
    AppState,
    api::{ApiReply, ApiResponse, input::ApiJson as Json},
};
use axum::extract::State;
use nuono_core::db::plugin_overrides::{OverridePatch, OverrideTarget};
use nuono_core::{auth::Capability, service::group_plugins as svc};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

#[utoipa::path(operation_id="group_plugins_list",get,path="/{group_id}/plugins",tag="group_plugins",params(("group_id"=String,Path)),responses((status=200,body=ApiResponse<Vec<svc::GroupPluginState>>)))]
pub async fn list(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
) -> ApiReply<Vec<svc::GroupPluginState>> {
    let actor = match state
        .manager
        .plugin_host
        .configuration_actor_for_group(&caller, group_id)
        .await
    {
        Ok(actor) => actor,
        Err(error) => return ApiResponse::from_err(&error),
    };
    match svc::list_for_group(
        &state.db,
        &state.manager.plugin_host,
        &state.plugin_config,
        group_id,
        &actor,
    )
    .await
    {
        Ok(value) => ApiResponse::ok(value),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(operation_id="group_plugins_save_config",post,path="/{group_id}/plugins/{name}/config",tag="group_plugins",params(("group_id"=String,Path),("name"=String,Path)),request_body=OverridePatch,responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn save_config(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, name)): crate::api::input::ApiPath<(i64, String)>,
    Json(patch): Json<OverridePatch>,
) -> ApiReply<serde_json::Value> {
    let actor = match state
        .manager
        .plugin_host
        .configuration_actor_for_group(&caller, group_id)
        .await
    {
        Ok(actor) => actor,
        Err(error) => return ApiResponse::from_err(&error),
    };
    match svc::save_override(
        &state.db,
        &state.manager.plugin_host,
        &state.plugin_config,
        group_id,
        &name,
        &actor,
        patch,
    )
    .await
    {
        Ok(value) => ApiResponse::ok(serde_json::json!(value)),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(operation_id="group_plugins_toggle",post,path="/{group_id}/plugins/{name}/toggle",tag="group_plugins",params(("group_id"=String,Path),("name"=String,Path)),request_body=OverridePatch,responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn toggle(
    state: State<AppState>,
    caller: crate::api::input::ApiCaller,
    path: crate::api::input::ApiPath<(i64, String)>,
    body: Json<OverridePatch>,
) -> ApiReply<serde_json::Value> {
    save_config(state, caller, path, body).await
}

#[utoipa::path(operation_id="group_plugins_clear_config",delete,path="/{group_id}/plugins/{name}/config",tag="group_plugins",params(("group_id"=String,Path),("name"=String,Path)),request_body=OverridePatch,responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn clear_config(
    State(state): State<AppState>,
    caller: crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, name)): crate::api::input::ApiPath<(i64, String)>,
    Json(mut patch): Json<OverridePatch>,
) -> ApiReply<serde_json::Value> {
    let row = match state
        .db
        .plugin_override(&name, OverrideTarget::Group(group_id))
        .await
    {
        Ok(row) => row,
        Err(error) => return ApiResponse::from_err(&error),
    };
    if let Some(row) = row {
        let parse = |raw: &str| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw)
                .map(|values| values.into_iter().map(|(key, _)| key).collect::<Vec<_>>())
        };
        patch.remove_config = match parse(&row.config_patch_json) {
            Ok(value) => value,
            Err(_) => return ApiResponse::bad_request("已保存配置格式无效"),
        };
        patch.remove_service = match parse(&row.service_patch_json) {
            Ok(value) => value,
            Err(_) => return ApiResponse::bad_request("已保存服务配置格式无效"),
        };
    }
    patch.set_config.clear();
    patch.set_service.clear();
    patch.set_enabled = None;
    patch.inherit_enabled = true;
    save_config(
        State(state),
        caller,
        crate::api::input::ApiPath((group_id, name)),
        Json(patch),
    )
    .await
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(toggle))
        .routes(routes!(save_config, clear_config))
        .routes(routes!(list_subscriptions, delete_subscription))
        .routes(routes!(list_authorizations, grant_authorization))
        .routes(routes!(list_user_authorizations))
        .routes(routes!(revoke_authorization))
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[schema(as = api::group_plugins::GroupPluginSubscriptionView)]
pub struct GroupPluginSubscriptionView {
    pub plugin_id: String,
    pub key: String,
    pub topic: String,
    pub platform: String,
    pub instance_id: String,
    pub bot_id: String,
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
    pub updated_at_ms: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::group_plugins::DeleteGroupSubscriptionBody)]
pub struct DeleteGroupSubscriptionBody {
    pub plugin_id: String,
    pub key: String,
}

/// 列出指定群内已开启的插件定时推送订阅（由插件通过 SDK `ctx.subscriptions()` 写入）。
#[utoipa::path(
    operation_id = "group_plugins_list_subscriptions",
    get,
    path = "/{group_id}/subscriptions",
    tag = "group_plugins",
    params(("group_id" = String, Path)),
    responses((status = 200, description = "该群当前生效的插件定时推送订阅列表", body = ApiResponse<Vec<GroupPluginSubscriptionView>>))
)]
pub async fn list_subscriptions(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
) -> ApiReply<Vec<GroupPluginSubscriptionView>> {
    let actor = match state
        .manager
        .plugin_host
        .configuration_actor_for_group(&caller, group_id)
        .await
    {
        Ok(actor) => actor,
        Err(error) => return ApiResponse::from_err(&error),
    };
    if let Err(error) = svc::ensure_group_settings_writable(&state.db, group_id, None, &actor).await
    {
        return ApiResponse::from_err(&error);
    }
    let group = match state.db.group_by_id(group_id).await {
        Ok(Some(group)) => group,
        Ok(None) => return ApiResponse::not_found("群对象不存在"),
        Err(error) => return ApiResponse::from_err(&error),
    };
    let rows = match state.db.list_group_plugin_subscriptions(&group).await {
        Ok(rows) => rows,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let mut visible = Vec::with_capacity(rows.len());
    for row in rows {
        if state
            .manager
            .plugin_host
            .plugin_role(&row.plugin_id)
            .await
            .is_service()
        {
            continue;
        }
        if svc::ensure_group_settings_writable(&state.db, group_id, Some(&row.plugin_id), &actor)
            .await
            .is_err()
        {
            continue;
        }
        visible.push(GroupPluginSubscriptionView {
            plugin_id: row.plugin_id,
            key: row.key,
            topic: row.topic,
            platform: row.platform,
            instance_id: row.instance_id,
            bot_id: row.bot_id,
            data: row.data,
            updated_at_ms: row.updated_at_ms.to_string(),
        });
    }
    ApiResponse::ok(visible)
}

/// 取消指定群内的一条插件定时推送订阅。
#[utoipa::path(
    operation_id = "group_plugins_delete_subscription",
    delete,
    path = "/{group_id}/subscriptions",
    tag = "group_plugins",
    params(("group_id" = String, Path)),
    request_body = DeleteGroupSubscriptionBody,
    responses((status = 200, description = "是否成功删除对应群的插件推送订阅", body = ApiResponse<bool>))
)]
pub async fn delete_subscription(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
    Json(body): Json<DeleteGroupSubscriptionBody>,
) -> ApiReply<bool> {
    if body.plugin_id.trim().is_empty() || body.key.trim().is_empty() {
        return ApiResponse::bad_request("plugin_id 与 key 不能为空");
    }
    if state
        .manager
        .plugin_host
        .plugin_role(&body.plugin_id)
        .await
        .is_service()
    {
        return ApiResponse::forbidden("底层服务插件不支持群级推送订阅管理");
    }
    let actor = match state
        .manager
        .plugin_host
        .configuration_actor_for_group(&caller, group_id)
        .await
    {
        Ok(actor) => actor,
        Err(error) => return ApiResponse::from_err(&error),
    };
    if let Err(error) =
        svc::ensure_group_settings_writable(&state.db, group_id, Some(&body.plugin_id), &actor)
            .await
    {
        return ApiResponse::from_err(&error);
    }
    if !matches!(
        actor.mode,
        nuono_app::configuration::ConfigurationMode::Administrator
    ) && !nuono_app::entitlement::is_group_active(group_id).await
    {
        return ApiResponse::forbidden("当前群组服务未激活");
    }
    let group = match state.db.group_by_id(group_id).await {
        Ok(Some(group)) => group,
        Ok(None) => return ApiResponse::not_found("群对象不存在"),
        Err(error) => return ApiResponse::from_err(&error),
    };
    match state
        .db
        .delete_group_plugin_subscription(&group, &body.plugin_id, &body.key)
        .await
    {
        Ok(deleted) => ApiResponse::ok(deleted),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GroupAuthorizationBody {
    user_id: String,
    plugin_id: Option<String>,
    source_key: String,
    valid_from_ms: String,
    expires_at_ms: Option<String>,
}

#[utoipa::path(operation_id="group_plugins_list_authorizations",get, path = "/{group_id}/authorizations", tag = "group_plugins",
    params(("group_id" = String, Path)),
    responses((status = 200, description = "该群各个独立授权来源", body = ApiResponse<Vec<super::docs::AnyJson>>)))]
pub async fn list_authorizations(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
) -> ApiReply<Vec<serde_json::Value>> {
    if !caller.has(Capability::ManageAllGroups) {
        return ApiResponse::forbidden(Capability::ManageAllGroups.denial_message());
    }
    match state.db.group_configuration_grants(group_id).await {
        Ok(grants) => ApiResponse::ok(grants.iter().map(authorization_json).collect()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(operation_id="group_plugins_list_user_authorizations",get, path = "/users/{user_id}/authorizations", tag = "group_plugins",
    params(("user_id" = i64, Path)),
    responses((status = 200, description = "指定账号名下的全部群组配置授权来源", body = ApiResponse<Vec<super::docs::AnyJson>>)))]
pub async fn list_user_authorizations(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(user_id): crate::api::input::ApiPath<i64>,
) -> ApiReply<Vec<serde_json::Value>> {
    if !caller.has(Capability::ManageAllGroups) {
        return ApiResponse::forbidden(Capability::ManageAllGroups.denial_message());
    }
    match state.db.user_configuration_grants(user_id).await {
        Ok(grants) => ApiResponse::ok(grants.iter().map(authorization_json).collect()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(operation_id="group_plugins_grant_authorization",post, path = "/{group_id}/authorizations", tag = "group_plugins", request_body = GroupAuthorizationBody,
    params(("group_id" = String, Path)),
    responses((status = 200, description = "人工授权；来源编号重试不重复签发", body = ApiResponse<super::docs::AnyJson>)))]
pub async fn grant_authorization(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
    Json(body): Json<GroupAuthorizationBody>,
) -> ApiReply<serde_json::Value> {
    if !caller.has(Capability::ManageAllGroups) {
        return ApiResponse::forbidden(Capability::ManageAllGroups.denial_message());
    }
    let Ok(user_id) = body.user_id.parse::<i64>() else {
        return ApiResponse::bad_request("账号 ID 无效");
    };
    let Ok(valid_from_ms) = body.valid_from_ms.parse::<i64>() else {
        return ApiResponse::bad_request("授权开始时间无效");
    };
    let expires_at_ms = match body
        .expires_at_ms
        .map(|value| value.parse::<i64>())
        .transpose()
    {
        Ok(value) => value,
        Err(_) => return ApiResponse::bad_request("授权到期时间无效"),
    };
    let grant = nuono_core::db::group_authorizations::ManualGroupGrant {
        user_id,
        group_id,
        plugin_id: body.plugin_id,
        source_key: body.source_key,
        valid_from_ms,
        expires_at_ms,
    };
    match state
        .db
        .grant_group_configuration(caller.user_id, &grant)
        .await
    {
        Ok(saved) => ApiResponse::ok(authorization_json(&saved)),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(operation_id="group_plugins_revoke_authorization",delete, path = "/{group_id}/authorizations/{authorization_id}", tag = "group_plugins",
    params(("group_id" = String, Path), ("authorization_id" = String, Path)),
    responses((status = 200, description = "只撤销指定来源，其他有效来源保留", body = ApiResponse<String>)))]
pub async fn revoke_authorization(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, id)): crate::api::input::ApiPath<(i64, String)>,
) -> ApiReply<String> {
    if !caller.has(Capability::ManageAllGroups) {
        return ApiResponse::forbidden(Capability::ManageAllGroups.denial_message());
    }
    let Ok(id) = id.parse::<i64>() else {
        return ApiResponse::bad_request("授权 ID 无效");
    };
    match state
        .db
        .revoke_group_configuration(caller.user_id, group_id, id)
        .await
    {
        Ok(()) => ApiResponse::ok("已撤销该授权来源".into()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

fn authorization_json(
    row: &nuono_core::db::group_authorizations::GroupAuthorization,
) -> serde_json::Value {
    serde_json::json!({
        "id": row.id.to_string(), "user_id": row.user_id.to_string(), "group_id": row.group_id.to_string(),
        "plugin_id": row.plugin_id, "issuer_kind": row.issuer_kind,
        "issuer_user_id": row.issuer_user_id.map(|id| id.to_string()), "issuer_plugin_id": row.issuer_plugin_id,
        "source_key": row.source_key, "valid_from_ms": row.valid_from_ms.to_string(),
        "expires_at_ms": row.expires_at_ms.map(|time| time.to_string()),
        "revoked_at_ms": row.revoked_at_ms.map(|time| time.to_string()), "revision": row.revision.to_string(),
    })
}
