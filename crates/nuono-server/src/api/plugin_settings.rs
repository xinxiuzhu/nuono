//! 插件全局策略、配置权限委派及旧版 TOML 配置预览与导入接口。
use crate::{
    AppState,
    api::{
        ApiReply, ApiResponse,
        input::{ApiCaller, ApiJson, ApiPath},
    },
};
use axum::extract::State;
use nuono_core::{
    config::plugin_config::{PluginImportPreview, PluginImportRequest},
    db::plugin_global_settings::PluginPolicyPatch,
};

#[utoipa::path(operation_id="plugin_settings_policy",get,path="/{name}/policy",tag="plugins",params(("name"=String,Path)),responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn policy(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
) -> ApiReply<serde_json::Value> {
    if state
        .manager
        .plugin_host
        .plugin_role(&name)
        .await
        .is_service()
        && !caller.is_owner()
    {
        return ApiResponse::forbidden("仅部署所有者可查看底层服务插件策略");
    }
    ApiResponse::from_result(state.db.plugin_policy(&name).await.map(|row|serde_json::json!({"plugin_id":row.plugin_id,"plugin_role":row.plugin_role,"enabled":row.enabled,"allow_user_overrides":row.allow_user_overrides,"allow_group_overrides":row.allow_group_overrides,"revision":row.revision.to_string(),"config_version":row.config_version.to_string()})))
}
#[utoipa::path(operation_id="plugin_settings_save_policy",post,path="/{name}/policy",tag="plugins",params(("name"=String,Path)),request_body=PluginPolicyPatch,responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn save_policy(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
    ApiJson(patch): ApiJson<PluginPolicyPatch>,
) -> ApiReply<serde_json::Value> {
    if let Err(error) = super::plugins::ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    match state
        .manager
        .plugin_host
        .save_policy_and_apply(caller.user_id, &name, patch)
        .await
    {
        Ok(()) => match state.db.plugin_policy(&name).await {
            Ok(current) => {
                ApiResponse::ok(serde_json::json!({"revision":current.revision.to_string()}))
            }
            Err(error) => ApiResponse::from_err(&error),
        },
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(operation_id="plugin_settings_preview_import",get,path="/{name}/config/import",tag="plugins",params(("name"=String,Path)),responses((status=200,body=ApiResponse<PluginImportPreview>)))]
pub async fn preview_import(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
) -> ApiReply<PluginImportPreview> {
    if let Err(error) = super::plugins::ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    ApiResponse::from_result(state.plugin_config.preview_import(&name).await)
}
#[utoipa::path(operation_id="plugin_settings_import",post,path="/{name}/config/import",tag="plugins",params(("name"=String,Path)),request_body=PluginImportRequest,responses((status=200,body=ApiResponse<super::docs::Empty>)))]
pub async fn import(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
    ApiJson(request): ApiJson<PluginImportRequest>,
) -> ApiReply<()> {
    if let Err(error) = super::plugins::ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    ApiResponse::from_result(
        state
            .plugin_config
            .import_selected(caller.user_id, &name, request)
            .await,
    )
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct DelegationBody {
    all_plugins: bool,
    plugin_ids: Vec<String>,
}
#[utoipa::path(operation_id="plugin_settings_read_delegation",get,path="/{name}/grant-delegation",tag="plugins",params(("name"=String,Path)),responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn read_delegation(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
) -> ApiReply<serde_json::Value> {
    let capability = nuono_core::auth::Capability::ManageRuntime;
    if !caller.has(capability) {
        return ApiResponse::forbidden(capability.denial_message());
    }
    if state
        .manager
        .plugin_host
        .plugin_role(&name)
        .await
        .is_service()
        && !caller.is_owner()
    {
        return ApiResponse::forbidden("仅部署所有者可查看底层服务插件委派配置");
    }
    if let Err(error) = state.db.plugin_policy(&name).await {
        return ApiResponse::from_err(&error);
    }
    match state
        .db
        .get_global_config(&format!("authorization.issuer.{name}"))
        .await
    {
        Ok(value) => match decode_delegation(value) {
            Ok(delegation) => ApiResponse::ok(serde_json::json!(delegation)),
            Err(error) => ApiResponse::internal(error),
        },
        Err(error) => ApiResponse::from_err(&error),
    }
}

fn decode_delegation(
    value: Option<serde_json::Value>,
) -> Result<nuono_app::group_authorization::PluginGrantDelegation, serde_json::Error> {
    value
        .map(serde_json::from_value)
        .transpose()
        .map(Option::unwrap_or_default)
}

#[utoipa::path(operation_id="plugin_settings_delegation",post,path="/{name}/grant-delegation",tag="plugins",params(("name"=String,Path)),request_body=DelegationBody,responses((status=200,body=ApiResponse<super::docs::Empty>)))]
pub async fn delegation(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
    ApiJson(body): ApiJson<DelegationBody>,
) -> ApiReply<()> {
    if state
        .manager
        .plugin_host
        .plugin_role(&name)
        .await
        .is_service()
        && !caller.is_owner()
    {
        return ApiResponse::forbidden("仅部署所有者可配置底层服务插件委派");
    }
    ApiResponse::from_result(
        state
            .db
            .set_plugin_grant_delegation(
                caller.user_id,
                &name,
                &nuono_app::group_authorization::PluginGrantDelegation {
                    all_plugins: body.all_plugins,
                    plugin_ids: body.plugin_ids,
                },
            )
            .await,
    )
}

#[cfg(test)]
mod tests {
    use super::decode_delegation;
    use serde_json::json;

    #[test]
    fn missing_delegation_is_empty_but_corrupt_saved_state_is_rejected() {
        assert!(!decode_delegation(None).unwrap().allows(Some("example")));
        assert!(decode_delegation(Some(json!({"all_plugins":true}))).is_err());
        let saved =
            decode_delegation(Some(json!({"all_plugins":false,"plugin_ids":["weather"]}))).unwrap();
        assert!(saved.allows(Some("weather")));
        assert!(!saved.allows(Some("other")));
        assert!(!saved.allows(None));
    }
}
